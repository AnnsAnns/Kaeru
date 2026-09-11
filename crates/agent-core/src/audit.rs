//! Append-only audit log (M3, ADR-019).
//!
//! Every tool execution and consent decision appends one JSON line to
//! `data/audit.jsonl`: turn id, tool, input, decision, exit status, duration
//! (and the worker model for worker calls). Frontends ignore it; the owner
//! greps it. Writes never fail a turn — an I/O error is logged and dropped.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One audit line. `status` is a short outcome word (`ok`, `error`, `denied`);
/// `decision` is present only when the tool went through the consent flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub turn_id: u64,
    /// Tool name, or `worker:<name>` for a worker sub-call (ADR-021).
    pub tool: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub input: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// Model the call ran with (worker calls record their per-worker model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub status: String,
    pub duration_ms: u64,
}

/// Handle to `data/audit.jsonl`. A disabled log (no path) drops entries.
#[derive(Debug, Clone, Default)]
pub struct AuditLog {
    path: Option<PathBuf>,
}

impl AuditLog {
    /// An audit log that records nothing (tests, headless core use).
    pub fn disabled() -> Self {
        Self { path: None }
    }

    /// An audit log writing to `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// Append one entry; a write failure is logged, never propagated.
    pub fn append(&self, entry: &AuditEntry) {
        let Some(path) = &self.path else {
            return;
        };
        if let Err(err) = self.write_line(path, entry) {
            tracing::warn!(
                target: "agent_core::audit",
                "cannot append to audit log {}: {err}",
                path.display()
            );
        }
    }

    fn write_line(&self, path: &std::path::Path, entry: &AuditEntry) -> std::io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(entry).unwrap_or_else(|_| "{}".into());
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kaeru-audit-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        dir.join("audit.jsonl")
    }

    #[test]
    fn appends_one_json_line_per_entry() {
        let path = temp_path("append");
        let log = AuditLog::new(&path);
        log.append(&AuditEntry {
            turn_id: 1,
            tool: "web_search".into(),
            input: serde_json::json!({"query": "frogs"}),
            decision: None,
            model: None,
            status: "ok".into(),
            duration_ms: 12,
        });
        log.append(&AuditEntry {
            turn_id: 1,
            tool: "memory_write".into(),
            input: serde_json::json!({"content": "note"}),
            decision: Some("deny".into()),
            model: None,
            status: "denied".into(),
            duration_ms: 3,
        });

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: AuditEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first.tool, "web_search");
        assert_eq!(first.status, "ok");
        let second: AuditEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second.decision.as_deref(), Some("deny"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn disabled_log_writes_nothing() {
        let log = AuditLog::disabled();
        log.append(&AuditEntry {
            turn_id: 1,
            tool: "web_search".into(),
            input: serde_json::Value::Null,
            decision: None,
            model: None,
            status: "ok".into(),
            duration_ms: 0,
        });
        assert!(!log.is_enabled());
    }
}
