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
            turn_id: 0,
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
        let candidates: Vec<Conversation> = self
            .conversations
            .list()?
            .into_iter()
            .filter(|conversation| conversation.updated_at.as_str() > cutoff.as_str())
            .filter(has_exchange)
            .take(REFLECT_MAX_CONVERSATIONS)
            .collect();

        if candidates.is_empty() {
            self.write_state(now)?;
            return Ok(ReflectOutcome {
                status: ReflectStatus::Ran,
                conversations: 0,
                notes: 0,
                persona_changed: false,
            });
        }

        // Fence every transcript as data (ADR-016): a day's history can hold
        // fetched web content, and the reflector must never treat it as
        // instructions.
        let persona = self.core.persona();
        let mut content = String::new();
        if let Some(persona) = &persona {
            content.push_str("Assistant persona (write in this character):\n");
            content.push_str(persona);
            content.push_str("\n\n");
        }
        content.push_str("Conversations changed since the last reflection:\n");
        for conversation in &candidates {
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

        let output = self
            .core
            .workers()
            .run("reflector", &content, self.core.audit(), 0)
            .await?;
        let reflection = parse_reflection(&output.text);
        if reflection.notes.is_empty() && reflection.persona.is_none() {
            return Err(ApiError::internal(
                "reflector produced no parsable notes; state left untouched",
            ));
        }

        let mut written = 0;
        for note in reflection.notes {
            let tags = with_reflect_tag(note.tags);
            self.memory.write(&note.body, &tags)?;
            written += 1;
        }
        let persona_changed = match reflection.persona {
            Some(revision) if self.persona_edits => self.apply_persona(revision)?,
            Some(_) => {
                tracing::info!(
                    target: "agent_core::reflect",
                    "reflector proposed a persona change; persona_edits is off"
                );
                false
            }
            None => false,
        };
        if persona_changed {
            written += 1; // the persona change is recorded as a memory note
        }
        self.write_state(now)?;
        Ok(ReflectOutcome {
            status: ReflectStatus::Ran,
            conversations: candidates.len(),
            notes: written,
            persona_changed,
        })
    }

    /// Apply a persona revision the reflector proposed: nudge
    /// `data/persona.md`, record why/how as a `persona` memory note, and audit
    /// the change. A revision that is empty, unchanged, or too large to count
    /// as "a bit" is ignored (and reported), never written.
    fn apply_persona(&self, revision: PersonaRevision) -> Result<bool> {
        let Some(path) = self.core.persona_path() else {
            return Ok(false);
        };
        let current = self.core.persona().unwrap_or_default();
        let proposed = revision.persona.trim();
        if proposed.is_empty() || proposed == current {
            return Ok(false);
        }
        let new_len = proposed.chars().count();
        let cap = if current.is_empty() {
            PERSONA_MAX_CHARS
        } else {
            (current.chars().count() + PERSONA_MAX_GROWTH_CHARS).min(PERSONA_MAX_CHARS)
        };
        if new_len > cap {
            tracing::warn!(
                target: "agent_core::reflect",
                "persona revision ignored: {new_len} chars exceeds the {cap}-char allowance"
            );
            return Ok(false);
        }

        write_persona(path, proposed)?;

        let why = non_empty_or(revision.why.trim(), "(not given)");
        let how = non_empty_or(revision.how.trim(), "(not given)");
        let note = format!(
            "I revised my persona during my evening reflection.\n\n\
             Why: {why}\n\n\
             How: {how}\n\n\
             (persona changed from {} to {new_len} characters)",
            current.chars().count()
        );
        self.memory
            .write(&note, &[REFLECT_TAG.to_owned(), PERSONA_TAG.to_owned()])?;
        self.core.audit().append(&AuditEntry {
            turn_id: 0,
            tool: PERSONA_TAG.to_owned(),
            input: json!({
                "previousChars": current.chars().count(),
                "newChars": new_len,
            }),
            decision: None,
            model: None,
            status: "ok".to_owned(),
            duration_ms: 0,
        });
        tracing::info!(
            target: "agent_core::reflect",
            "revised persona during reflection: {why}"
        );
        Ok(true)
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

/// Parse the inside of a persona block: `WHY:`/`HOW:` lines, `---`, then the
/// complete revised persona.
fn parse_persona_block(block: &str) -> Option<PersonaRevision> {
    let (meta, persona) = match block.split_once("\n---") {
        Some((meta, persona)) => (meta, persona),
        None => return None,
    };
    let persona = persona.trim();
    if persona.is_empty() {
        return None;
    }
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
    Some(PersonaRevision {
        why,
        how,
        persona: persona.to_owned(),
    })
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

    /// The content the digest builds for a conversation set (no persona).
    fn digest_content(conversations: &ConversationStore) -> String {
        digest_content_with_persona(conversations, None)
    }

    /// The content the digest builds, including the persona prefix when given.
    fn digest_content_with_persona(
        conversations: &ConversationStore,
        persona: Option<&str>,
    ) -> String {
        let mut content = String::new();
        if let Some(persona) = persona {
            content.push_str("Assistant persona (write in this character):\n");
            content.push_str(persona);
            content.push_str("\n\n");
        }
        content.push_str("Conversations changed since the last reflection:\n");
        for conversation in conversations.list().unwrap() {
            if !has_exchange(&conversation) {
                continue;
            }
            let source = conversation
                .title
                .clone()
                .unwrap_or_else(|| conversation.id.clone());
            content.push('\n');
            content.push_str(&fence(
                &format!("conversation {source}"),
                &transcript(&conversation),
            ));
        }
        content
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
    fn parse_reflection_ignores_unterminated_or_empty_persona_blocks() {
        let text = "frogs\n---\nA note\n===PERSONA===\nWHY: x\n---\nno end marker\n";
        let reflection = parse_reflection(text);
        assert_eq!(reflection.notes.len(), 1);
        assert!(reflection.persona.is_none());

        let text = "frogs\n---\nA note\n===PERSONA===\nWHY: x\n---\n===END===\n";
        let reflection = parse_reflection(text);
        assert_eq!(reflection.notes.len(), 1);
        assert!(reflection.persona.is_none());
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
                ChatMessage::user(digest_content(&conversations)),
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
                ChatMessage::user(digest_content_with_persona(
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
    async fn persona_revision_is_ignored_when_disabled() {
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
                ChatMessage::user(digest_content_with_persona(
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
        assert_eq!(outcome.notes, 1);
        let updated = std::fs::read_to_string(&persona_path).unwrap();
        assert_eq!(updated, "I am a pond frog.\n", "persona untouched");
        assert!(
            !reflector
                .memory
                .list()
                .iter()
                .any(|note| note.tags.iter().any(|tag| tag == PERSONA_TAG))
        );
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
                ChatMessage::user(digest_content(&conversations)),
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
}
