//! Consent-gated memory persistence (M3 write side; M4 enriches with tags,
//! search, injection and the browser).
//!
//! Memory survives across sessions, so a silent write is a prompt-injection
//! vector: `memory_write` always routes through the consent flow (ADR-016).
//! This module owns the plain-markdown store the tool writes to.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::error::{ApiError, Result};
use crate::events::{ApprovalKind, Risk};
use crate::tools::{Tool, ToolContext, ToolFuture};

/// Markdown memory store rooted at a day-partitioned directory (`data/memory`).
#[derive(Debug, Clone)]
pub struct MemoryStore {
    dir: PathBuf,
}

impl MemoryStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The directory for today's local date (`<dir>/YYYY-MM-DD`).
    pub fn day_dir(&self) -> PathBuf {
        let today: String = crate::conversations::now_rfc3339()
            .chars()
            .take(10)
            .collect();
        self.dir.join(today)
    }

    /// Persist one markdown note for today; returns the written path.
    pub fn write(&self, content: &str, tags: &[String]) -> Result<PathBuf> {
        let dir = self.day_dir();
        std::fs::create_dir_all(&dir).map_err(|e| {
            ApiError::internal(format!("cannot create memory dir {}: {e}", dir.display()))
        })?;
        let path = dir.join(format!("{}.md", self.file_stem(content)));
        let tags = if tags.is_empty() {
            "[]".to_owned()
        } else {
            format!("[{}]", tags.join(", "))
        };
        let created: String = crate::conversations::now_rfc3339()
            .chars()
            .take(10)
            .collect();
        let document = format!(
            "---\ntags: {tags}\ncreated: {created}\n---\n{}\n",
            content.trim()
        );
        std::fs::write(&path, document).map_err(|e| {
            ApiError::internal(format!("cannot write memory {}: {e}", path.display()))
        })?;
        Ok(path)
    }

    fn file_stem(&self, content: &str) -> String {
        let first_line = content.lines().next().unwrap_or("").trim();
        let mut slug: String = first_line
            .chars()
            .take(40)
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        while slug.contains("--") {
            slug = slug.replace("--", "-");
        }
        let slug = slug.trim_matches('-');
        let slug = if slug.is_empty() { "note" } else { slug };
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{slug}-{nanos:x}")
    }
}

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

    fn execute(&self, input: Value, _ctx: ToolContext) -> ToolFuture {
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
        let tags: Vec<String> = input
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
            let path = store.write(&content, &tags)?;
            Ok(format!("Saved memory to {}", path.display()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ApprovalKind;

    fn temp_store(name: &str) -> MemoryStore {
        let dir = std::env::temp_dir().join(format!("kaeru-memory-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        MemoryStore::new(dir)
    }

    #[test]
    fn write_creates_a_day_file_with_frontmatter() {
        let store = temp_store("write");
        let path = store
            .write("Rust notes", &["rust".into(), "axum".into()])
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\n"));
        assert!(text.contains("tags: [rust, axum]"));
        assert!(text.contains("Rust notes"));
        assert!(path.starts_with(store.day_dir()));
        std::fs::remove_dir_all(store.dir()).ok();
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
}
