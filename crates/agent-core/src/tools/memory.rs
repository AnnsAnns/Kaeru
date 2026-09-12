//! The memory tools (M3 write side, M4 read side).
//!
//! Memory survives across sessions, so a silent write is a prompt-injection
//! vector: `memory_write` always routes through the consent flow (ADR-016) and
//! the note is auto-tagged/summarized by the tool-free `distiller` worker
//! (ADR-021). `memory_search` is read-only and therefore safe (ADR-007).

use serde_json::{Value, json};

use crate::agent::fence;
use crate::error::ApiError;
use crate::events::{ApprovalKind, Risk};
use crate::memory::store::MemoryStore;
use crate::tools::{Tool, ToolContext, ToolFuture};
use crate::util::parse_tag_line;

/// Most notes a single `memory_search` returns.
const MAX_SEARCH_RESULTS: usize = 20;
const DEFAULT_SEARCH_RESULTS: usize = 5;

/// The `memory_write` tool: consent-gated persistence (ADR-016).
pub struct MemoryWriteTool {
    store: MemoryStore,
}

impl MemoryWriteTool {
    pub fn new(store: MemoryStore) -> Self {
        Self { store }
    }
}

impl Tool for MemoryWriteTool {
    fn name(&self) -> &'static str {
        "memory_write"
    }

    fn description(&self) -> &'static str {
        "Save a durable note to the user's memory. Requires the user's explicit \
         consent because memory persists across sessions."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The note to remember, in plain text or Markdown."
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional tags (e.g. project names)."
                }
            },
            "required": ["content"]
        })
    }

    /// Memory writes always need consent (persists across sessions).
    fn risk(&self, _input: &Value) -> Risk {
        Risk::NeedsApproval(ApprovalKind::MemoryWrite {
            path: self.store.day_dir().display().to_string(),
        })
    }

    fn approval_summary(&self, input: &Value) -> String {
        let preview = input
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let preview: String = preview.chars().take(160).collect();
        format!(
            "Save a memory entry to {} — {:?}",
            self.store.day_dir().display(),
            preview
        )
    }

    fn execute(&self, input: Value, ctx: ToolContext) -> ToolFuture {
        let content = match input.get("content").and_then(Value::as_str) {
            Some(content) if !content.trim().is_empty() => content.trim().to_owned(),
            _ => {
                return Box::pin(async {
                    Err(ApiError::config(
                        "memory_write requires non-empty \"content\"",
                    ))
                });
            }
        };
        let explicit: Vec<String> = input
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let store = self.store.clone();
        Box::pin(async move {
            // The distiller (tool-free, ADR-021) tidies the candidate into a
            // durable note + tags. It is optional: if it is unavailable or
            // fails, the consented text is stored verbatim.
            let (content, mut tags) = if ctx.workers.distiller().is_some() {
                let fenced = fence("memory_candidate", &content);
                match ctx
                    .workers
                    .run("distiller", &fenced, &ctx.audit, Some(ctx.turn_id))
                    .await
                {
                    Ok(output) => {
                        let (distilled_tags, body) = parse_distilled(&output.text);
                        (body, distilled_tags)
                    }
                    Err(err) => {
                        tracing::warn!(
                            target: "agent_core::memory",
                            "distiller failed ({}); storing the note verbatim: {}",
                            err.kind.as_str(),
                            err.message
                        );
                        (content.clone(), Vec::new())
                    }
                }
            } else {
                (content.clone(), Vec::new())
            };
            tags = merge_tags(explicit, tags);
            let path = store.write(&content, &tags)?;
            Ok(format!("Saved memory to {}", path.display()))
        })
    }
}

/// The `memory_search` tool: read-only recall of durable notes (safe).
pub struct MemorySearchTool {
    store: MemoryStore,
}

impl MemorySearchTool {
    pub fn new(store: MemoryStore) -> Self {
        Self { store }
    }
}

impl Tool for MemorySearchTool {
    fn name(&self) -> &'static str {
        "memory_search"
    }

    fn description(&self) -> &'static str {
        "Search the user's durable memory notes (things they asked you to \
         remember). Use this before asking the user to repeat themselves."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Words to look for in memory notes."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of notes to return (default 5)."
                }
            },
            "required": ["query"]
        })
    }

    /// Read-only local recall: safe to run without consent.
    fn risk(&self, _input: &Value) -> Risk {
        Risk::Safe
    }

    fn execute(&self, input: Value, _ctx: ToolContext) -> ToolFuture {
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_owned);
        let query = match query {
            Some(query) => query,
            None => {
                return Box::pin(async {
                    Err(ApiError::config(
                        "memory_search requires a non-empty \"query\"",
                    ))
                });
            }
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|limit| limit as usize)
            .unwrap_or(DEFAULT_SEARCH_RESULTS)
            .clamp(1, MAX_SEARCH_RESULTS);
        let store = self.store.clone();
        Box::pin(async move {
            let hits = store.search(&query, limit);
            if hits.is_empty() {
                return Ok(format!("No memory notes matched {query:?}."));
            }
            let mut out = format!("{} memory note(s) for {query:?}:", hits.len());
            for note in hits {
                let tags = if note.tags.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", note.tags.join(", "))
                };
                out.push_str(&format!(
                    "\n\n[{}]{} {}",
                    note.day,
                    tags,
                    note.content.trim()
                ));
            }
            Ok(out)
        })
    }
}

/// Merge explicit and distilled tags, preserving order and dropping exact
/// duplicates.
fn merge_tags(explicit: Vec<String>, distilled: Vec<String>) -> Vec<String> {
    let mut merged = explicit;
    for tag in distilled {
        let tag = tag.trim().to_owned();
        if !tag.is_empty() && !merged.iter().any(|existing| existing == &tag) {
            merged.push(tag);
        }
    }
    merged
}

/// Parse the distiller's `tags\n---\nnote` output, leniently: a missing
/// separator means "no tags, the whole text is the note".
fn parse_distilled(text: &str) -> (Vec<String>, String) {
    let mut tags = Vec::new();
    let mut body = String::new();
    let mut in_body = false;
    for line in text.lines() {
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
        // No separator (or nothing after it): keep the text as the note.
        return (Vec::new(), text.trim().to_owned());
    }
    let tags = parse_tag_line(&tags.join(" "));
    (tags, body.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::workers::{DISTILLER_SYSTEM, Workers};
    use crate::audit::AuditLog;
    use crate::config::{DEFAULT_MODEL, WorkersConfig};
    use crate::events::CoreEvent;
    use crate::llm::{Cassette, ChatMessage, ChatRequest, FakeProvider, Interaction};
    use std::sync::Arc;

    fn temp_store(name: &str) -> MemoryStore {
        let dir = std::env::temp_dir().join(format!("kaeru-memory-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        MemoryStore::new(dir)
    }

    fn context_with_distiller(candidate: &str, events: Option<Vec<CoreEvent>>) -> ToolContext {
        let model = DEFAULT_MODEL;
        let interactions = events
            .map(|events| {
                vec![Interaction {
                    request: ChatRequest::new(
                        model,
                        vec![
                            ChatMessage::system(DISTILLER_SYSTEM),
                            ChatMessage::user(fence("memory_candidate", candidate)),
                        ],
                    )
                    .with_max_tokens(Some(600)),
                    events,
                }]
            })
            .unwrap_or_default();
        let client: Arc<dyn crate::llm::LlmClient> =
            Arc::new(FakeProvider::from_cassette(Cassette {
                interactions,
                ..Cassette::new()
            }));
        let workers = Workers::from_config(Arc::clone(&client), model, &WorkersConfig::default());
        ToolContext {
            client,
            search: Arc::new(crate::search::DisabledSearch),
            workers: Arc::new(workers),
            audit: AuditLog::disabled(),
            turn_id: 1,
        }
    }

    #[test]
    fn memory_write_is_consent_gated() {
        let tool = MemoryWriteTool::new(temp_store("risk"));
        let risk = tool.risk(&json!({"content": "hi"}));
        assert!(matches!(
            risk,
            Risk::NeedsApproval(ApprovalKind::MemoryWrite { .. })
        ));
        let summary = tool.approval_summary(&json!({"content": "remember frogs"}));
        assert!(summary.contains("remember frogs"));
    }

    #[tokio::test]
    async fn memory_write_stores_verbatim_when_no_distiller_runs() {
        let store = temp_store("verbatim");
        let tool = MemoryWriteTool::new(store.clone());
        let mut ctx = context_with_distiller("raw candidate text", None);
        // Disabled registry: no distiller, so no provider call at all.
        let client = Arc::clone(&ctx.client);
        ctx.workers = Arc::new(Workers::disabled(client));
        let input = json!({"content": "remember frogs", "tags": ["animals"]});
        tool.execute(input, ctx).await.unwrap();
        let notes = store.list();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "remember frogs");
        assert_eq!(notes[0].tags, vec!["animals".to_string()]);
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[tokio::test]
    async fn memory_write_uses_the_distiller_for_summary_and_tags() {
        let store = temp_store("distill");
        let tool = MemoryWriteTool::new(store.clone());
        let ctx = context_with_distiller(
            "raw candidate text",
            Some(vec![
                CoreEvent::Delta {
                    text: "rust\n---\nThe user prefers short Rust notes.".into(),
                },
                CoreEvent::TurnDone { usage: None },
            ]),
        );
        // The fake provider matches on the request, so any content works.
        let input = json!({"content": "raw candidate text", "tags": ["explicit"]});
        tool.execute(input, ctx).await.unwrap();
        let notes = store.list();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "The user prefers short Rust notes.");
        assert_eq!(
            notes[0].tags,
            vec!["explicit".to_string(), "rust".to_string()]
        );
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[tokio::test]
    async fn memory_write_falls_back_when_the_distiller_fails() {
        let store = temp_store("distill-fail");
        let tool = MemoryWriteTool::new(store.clone());
        let ctx = context_with_distiller(
            "keep me anyway",
            Some(vec![CoreEvent::error(
                crate::error::ApiErrorKind::Provider,
                "boom",
            )]),
        );
        let input = json!({"content": "keep me anyway"});
        tool.execute(input, ctx).await.unwrap();
        let notes = store.list();
        assert_eq!(notes[0].content, "keep me anyway");
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[tokio::test]
    async fn memory_search_returns_matches_and_is_safe() {
        let store = temp_store("search");
        store
            .write("frogs live in ponds", &["animals".into()])
            .unwrap();
        let tool = MemorySearchTool::new(store.clone());
        let ctx = context_with_distiller("unused", None);

        let output = tool
            .execute(json!({"query": "frogs"}), ctx.clone())
            .await
            .unwrap();
        assert!(output.contains("frogs live in ponds"));
        assert!(matches!(tool.risk(&json!({"query": "x"})), Risk::Safe));

        let miss = tool
            .execute(json!({"query": "spaceships"}), ctx.clone())
            .await
            .unwrap();
        assert!(miss.contains("No memory notes matched"));

        let err = tool.execute(json!({}), ctx).await.unwrap_err();
        assert_eq!(err.kind, crate::error::ApiErrorKind::Config);
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn parse_distilled_handles_missing_separator_and_tag_noise() {
        let (tags, body) = parse_distilled("tags: rust, axum\n---\nA note");
        assert_eq!(tags, vec!["rust".to_string(), "axum".to_string()]);
        assert_eq!(body, "A note");
        let (tags, body) = parse_distilled("no separator here");
        assert!(tags.is_empty());
        assert_eq!(body, "no separator here");
    }
}
