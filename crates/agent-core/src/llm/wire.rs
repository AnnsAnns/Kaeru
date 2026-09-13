//! OpenAI-compatible wire forms (Appendix A, ADR-005). Unknown fields are
//! ignored on purpose: providers routinely extend chunks with extra keys.

use serde::{Deserialize, Serialize};

use crate::events::Usage;

use super::types::{ChatMessage, ChatRequest, Role, ToolCall};

/// POST `/chat/completions` request body.
#[derive(Debug, Serialize)]
pub(crate) struct WireChatRequest {
    pub model: String,
    pub messages: Vec<WireChatMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<WireStreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl WireChatRequest {
    /// The streaming form used for chat turns. `stream_options.include_usage`
    /// asks OpenAI-spec servers to attach usage to the final chunk; servers
    /// that do not send it are tolerated (usage stays `None`).
    pub(crate) fn streaming(request: &ChatRequest) -> Self {
        Self {
            model: request.model.clone(),
            messages: request.messages.iter().map(WireChatMessage::from).collect(),
            stream: true,
            stream_options: Some(WireStreamOptions {
                include_usage: true,
            }),
            reasoning_effort: request.reasoning_effort.clone(),
            tools: request.tools.clone(),
            max_tokens: request.max_tokens,
        }
    }
}

/// A message as sent to the provider: `role` + `content`, plus tool-call
/// fields on the agent loop (M3). Model thinking (`ChatMessage::reasoning`) is
/// display-only and never sent back. An assistant message that only requests
/// tools sends `content: null` (the OpenAI shape).
#[derive(Debug, Serialize)]
pub(crate) struct WireChatMessage {
    pub role: Role,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl From<&ChatMessage> for WireChatMessage {
    fn from(message: &ChatMessage) -> Self {
        let tool_calls = message.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|call| WireToolCall {
                    id: call.id.clone(),
                    r#type: "function",
                    function: WireFunctionCall {
                        name: call.name.clone(),
                        arguments: serde_json::to_string(&call.arguments)
                            .unwrap_or_else(|_| "{}".into()),
                    },
                })
                .collect()
        });
        // A tool-only assistant turn carries no prose: send null, not "".
        let content = if message.content.is_empty() && tool_calls.is_some() {
            None
        } else {
            Some(message.content.clone())
        };
        Self {
            role: message.role,
            content,
            tool_calls,
            tool_call_id: message.tool_call_id.clone(),
        }
    }
}

/// OpenAI `tool_calls` entry on an outbound message.
#[derive(Debug, Serialize)]
pub(crate) struct WireToolCall {
    pub id: String,
    pub r#type: &'static str,
    pub function: WireFunctionCall,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct WireStreamOptions {
    pub include_usage: bool,
}

/// One streaming chunk (`data: {...}` SSE payload).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireChunk {
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireChoice {
    #[serde(default)]
    pub delta: WireDelta,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireDelta {
    #[serde(default)]
    pub content: Option<String>,
    /// Thinking text; DeepSeek/OpenRouter name it `reasoning_content`, other
    /// OpenRouter models use `reasoning`.
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Streaming tool-call fragments (M3); accumulated by the adapter.
    #[serde(default)]
    pub tool_calls: Vec<WireDeltaToolCall>,
}

impl WireDelta {
    /// The provider's thinking chunk, preferring `reasoning_content`.
    pub(crate) fn reasoning_text(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
    }
}

/// One fragment of a streamed tool call. `index` ties the fragments of one
/// call together; `name` usually arrives whole in the first fragment, while
/// `arguments` is split across many.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireDeltaToolCall {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<WireDeltaFunction>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireDeltaFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Provider usage; OpenAI names (`prompt_tokens`, `completion_tokens`).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
}

impl From<WireUsage> for Usage {
    fn from(u: WireUsage) -> Self {
        Self {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            total_tokens: u
                .total_tokens
                .or_else(|| match (u.prompt_tokens, u.completion_tokens) {
                    (Some(i), Some(o)) => Some(i + o),
                    _ => None,
                }),
        }
    }
}

/// Full (non-streaming) completion response, used as fallback when a provider
/// answers `application/json` despite `stream: true`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireCompletion {
    #[serde(default)]
    pub choices: Vec<WireCompletionChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireCompletionChoice {
    #[serde(default)]
    pub message: WireMessage,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Complete tool calls on a non-streaming completion (M3).
    #[serde(default)]
    pub tool_calls: Vec<WireMessageToolCall>,
}

impl WireMessage {
    /// The provider's thinking text, preferring `reasoning_content`.
    pub(crate) fn reasoning_text(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
    }

    /// The message's tool calls in model form (arguments parsed from JSON).
    pub(crate) fn tool_calls(&self) -> Vec<ToolCall> {
        self.tool_calls
            .iter()
            .map(|call| ToolCall {
                id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: serde_json::from_str(&call.function.arguments)
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect()
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireMessageToolCall {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub function: WireMessageFunction,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireMessageFunction {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

/// OpenAI-style error body: `{"error": {"message": ...}}`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireErrorBody {
    #[serde(default)]
    pub error: Option<WireErrorDetail>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireErrorDetail {
    #[serde(default)]
    pub message: Option<String>,
}

/// GET `/models` response: `{"data": [{"id": ...}, ...]}`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireModelList {
    #[serde(default)]
    pub data: Vec<WireModel>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireModel {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub reasoning: Option<WireReasoning>,
}

/// Per-model reasoning metadata (Charm Hyper/OpenRouter style): the effort
/// levels a model accepts and its default.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireReasoning {
    #[serde(default)]
    pub effort_levels: Vec<WireEffortLevel>,
    #[serde(default)]
    pub default_effort_level: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct WireEffortLevel {
    #[serde(default)]
    pub value: Option<String>,
}


#[cfg(test)]
mod tests;
