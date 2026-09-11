//! The tool seam: a self-describing [`Tool`] trait, a registry, and the M3
//! tools (`web_search`, consent-gated `memory_write`). Every tool declares its
//! [`Risk`]; risky tools route through the consent flow (ADR-014).

pub mod memory;
pub mod web_search;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::agent::workers::Workers;
use crate::audit::AuditLog;
use crate::error::Result;
use crate::events::Risk;
use crate::llm::LlmClient;
use crate::search::SearchProvider;

pub use memory::{MemoryStore, MemoryWriteTool};
pub use web_search::WebSearchTool;

/// A tool execution future.
pub type ToolFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;

/// Everything a tool may use. Workers deliberately never receive a
/// `ToolContext` (C16).
#[derive(Clone)]
pub struct ToolContext {
    pub client: Arc<dyn LlmClient>,
    pub search: Arc<dyn SearchProvider>,
    pub workers: Arc<Workers>,
    pub audit: AuditLog,
    /// Turn id for audit entries.
    pub turn_id: u64,
}

/// A capability the model can call (plan §5.2). `schema()` returns the OpenAI
/// `tools` entry; `risk()` decides whether the call needs consent.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON-schema parameters object.
    fn parameters(&self) -> Value;

    /// The OpenAI `tools` entry (Appendix A, M3 additions).
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name(),
                "description": self.description(),
                "parameters": self.parameters(),
            }
        })
    }

    /// `Safe` tools run immediately; `NeedsApproval` pauses for consent.
    fn risk(&self, input: &Value) -> Risk;

    /// One-line text for the consent card; names the source of the request
    /// (ADR-016). Only used when [`Tool::risk`] is `NeedsApproval`.
    fn approval_summary(&self, input: &Value) -> String {
        format!(
            "Allow tool `{}` with input {}?",
            self.name(),
            summarize_input(input)
        )
    }

    /// Run the tool; errors become structured `ToolResult{is_error}` the model
    /// can react to (never a crash).
    fn execute(&self, input: Value, ctx: ToolContext) -> ToolFuture;
}

/// Updated after a successful call, so the UI can show which tools ran.
fn summarize_input(input: &Value) -> String {
    let text = serde_json::to_string(input).unwrap_or_default();
    if text.chars().count() <= 120 {
        text
    } else {
        format!("{}…", text.chars().take(120).collect::<String>())
    }
}

/// The set of tools a turn may call.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// The M3 default set: `web_search` (always) and `memory_write` (only when
    /// a memory store is configured).
    pub fn with_defaults(max_search_results: usize, memory: Option<MemoryStore>) -> Self {
        let mut registry = Self::new();
        registry.register(WebSearchTool::new(max_search_results));
        if let Some(store) = memory {
            registry.register(MemoryWriteTool::new(store));
        }
        registry
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.push(Arc::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.name() == name)
            .map(Arc::clone)
    }

    /// The OpenAI `tools` array advertised on a request.
    pub fn schemas(&self) -> Vec<Value> {
        self.tools.iter().map(|tool| tool.schema()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
            "echoes its input"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        fn risk(&self, _input: &Value) -> Risk {
            Risk::Safe
        }
        fn execute(&self, input: Value, _ctx: ToolContext) -> ToolFuture {
            Box::pin(async move { Ok(serde_json::to_string(&input).unwrap()) })
        }
    }

    #[test]
    fn registry_exposes_openai_tool_schemas() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        let schema = &registry.schemas()[0];
        assert_eq!(schema["type"], "function");
        assert_eq!(schema["function"]["name"], "echo");
        assert!(schema["function"]["parameters"]["properties"]["text"].is_object());
        assert!(registry.get("echo").is_some());
        assert!(registry.get("missing").is_none());
    }
}
