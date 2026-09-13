//! Chat types: the core-level request vocabulary and the OpenAI-compatible
//! wire forms (Appendix A). All provider protocol knowledge lives in `llm/`
//! (ADR-005); nothing else in the core or frontends may depend on it.

use serde::{Deserialize, Serialize};

use crate::events::Artifact;

/// Chat roles as used by the conversation history (OpenAI lowercase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}

impl Role {
    /// OpenAI wire name (lowercase, matches the serde form).
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// One conversation message. `Tool` and assistant `tool_calls` arrive with the
/// agent loop (M3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Model "thinking" for assistant turns (`reasoning_content` on
    /// DeepSeek/OpenRouter-style providers). Display-only: it is shown in the
    /// UI but never sent back to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Tool calls the assistant requested (assistant messages, M3). Sent back
    /// to the provider verbatim so the following `tool` messages line up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Which tool call this message answers (tool messages, M3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Workspace files attached to this message (M5): user uploads on a user
    /// message, tool outputs on the assistant answer. Display-only: never
    /// sent to the provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
            artifacts: Vec::new(),
        }
    }

    /// Attach model thinking to a message (assistant turns).
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }

    /// Attach workspace files to a message (M5).
    pub fn with_artifacts(mut self, artifacts: Vec<Artifact>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// An assistant message that requested tool calls (M3).
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) -> Self {
        let mut message = Self::new(Role::Assistant, content);
        message.tool_calls = Some(tool_calls);
        message
    }

    /// A `tool` result message answering `tool_call_id` (M3).
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut message = Self::new(Role::Tool, content);
        message.tool_call_id = Some(tool_call_id.into());
        message
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }
}

/// A tool call the model requested (M3): the model-level form of the
/// OpenAI `tool_calls` entry. `arguments` is the parsed JSON object; the wire
/// form carries it as a JSON-encoded string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

/// A turn request as the session layer builds it. Transport details
/// (`stream`, `stream_options`) are filled in by the HTTP adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    /// Optional reasoning effort for models that support it (`reasoning_effort`
    /// on OpenRouter/OpenAI-style providers). `None` leaves it to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// OpenAI `tools` entries the model may call (M3). Empty = no tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<serde_json::Value>,
    /// Optional output cap in tokens (bounded worker calls, M3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            reasoning_effort: None,
            tools: Vec::new(),
            max_tokens: None,
        }
    }

    /// Builder: request a reasoning effort; blank values are ignored.
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort.filter(|e| !e.trim().is_empty());
        self
    }

    /// Builder: advertise the given OpenAI `tools` entries.
    pub fn with_tools(mut self, tools: Vec<serde_json::Value>) -> Self {
        self.tools = tools;
        self
    }

    /// Builder: cap the provider's output.
    pub fn with_max_tokens(mut self, max_tokens: Option<u32>) -> Self {
        self.max_tokens = max_tokens.filter(|t| *t > 0);
        self
    }
}

/// One entry of the provider's model list. `id` is what the UI needs; the
/// optional effort levels let it offer the right reasoning choices per model
/// (Charm Hyper reports them under `reasoning.effort_levels`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_effort_levels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
}

impl ModelInfo {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            reasoning_effort_levels: Vec::new(),
            default_reasoning_effort: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_serialize_lowercase() {
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
    }
}
