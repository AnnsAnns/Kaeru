//! Chat types: the core-level request vocabulary and the OpenAI-compatible
//! wire forms (Appendix A). All provider protocol knowledge lives in `llm/`
//! (ADR-005); nothing else in the core or frontends may depend on it.

use serde::{Deserialize, Serialize};

use crate::events::Usage;

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
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            reasoning: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Attach model thinking to a message (assistant turns).
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
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

// ---------------------------------------------------------------------------
// OpenAI-compatible wire forms (Appendix A). Unknown fields are ignored on
// purpose: providers routinely extend chunks with extra keys.
// ---------------------------------------------------------------------------

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
mod tests {
    use super::*;

    #[test]
    fn streaming_request_shape_matches_the_openai_protocol() {
        let request = ChatRequest::new(
            "openai/gpt-4o-mini",
            vec![ChatMessage::user("Hello"), ChatMessage::assistant("Hi!")],
        );
        let json = serde_json::to_value(WireChatRequest::streaming(&request)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "model": "openai/gpt-4o-mini",
                "messages": [
                    {"role": "user", "content": "Hello"},
                    {"role": "assistant", "content": "Hi!"}
                ],
                "stream": true,
                "stream_options": {"include_usage": true}
            })
        );
    }

    #[test]
    fn tool_call_messages_serialize_with_null_content_and_tools() {
        let request = ChatRequest::new(
            "m",
            vec![
                ChatMessage::user("hi"),
                ChatMessage::assistant_with_tool_calls(
                    "",
                    vec![ToolCall::new(
                        "call_1",
                        "web_search",
                        serde_json::json!({"query": "x"}),
                    )],
                ),
                ChatMessage::tool("call_1", "result"),
            ],
        )
        .with_tools(vec![serde_json::json!({"type": "function"})]);
        let json = serde_json::to_value(WireChatRequest::streaming(&request)).unwrap();
        assert_eq!(
            json["messages"][1],
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "web_search", "arguments": "{\"query\":\"x\"}"}
                }]
            })
        );
        assert_eq!(
            json["messages"][2],
            serde_json::json!({"role": "tool", "content": "result", "tool_call_id": "call_1"})
        );
        assert_eq!(json["tools"][0]["type"], "function");
    }

    #[test]
    fn parses_content_delta_chunk() {
        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
        )
        .unwrap();
        assert_eq!(chunk.choices.len(), 1);
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hel"));
        assert!(chunk.usage.is_none());
    }

    #[test]
    fn tolerates_null_and_missing_content() {
        let chunk: WireChunk = serde_json::from_str(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null}}]}"#,
        )
        .unwrap();
        assert_eq!(chunk.choices[0].delta.content, None);

        let chunk: WireChunk =
            serde_json::from_str(r#"{"choices":[{"index":0,"delta":{}}]}"#).unwrap();
        assert_eq!(chunk.choices[0].delta.content, None);
    }

    #[test]
    fn parses_usage_only_final_chunk() {
        let chunk: WireChunk =
            serde_json::from_str(r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14}}"#)
                .unwrap();
        let usage = Usage::from(chunk.usage.unwrap());
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                total_tokens: Some(14),
            }
        );
    }

    #[test]
    fn usage_total_defaults_to_sum_when_missing() {
        let chunk: WireChunk =
            serde_json::from_str(r#"{"usage":{"prompt_tokens":7,"completion_tokens":3}}"#).unwrap();
        let usage = Usage::from(chunk.usage.unwrap());
        assert_eq!(usage.total_tokens, Some(10));
    }

    #[test]
    fn ignores_provider_specific_extra_fields() {
        let chunk: WireChunk = serde_json::from_str(
            r#"{"id":"gen-1","provider":"openrouter","choices":[{"index":0,"delta":{"content":"x"}}],"system_fingerprint":null}"#,
        )
        .unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("x"));
    }

    #[test]
    fn parses_non_streaming_completion() {
        let completion: WireCompletion = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":"Hello there"}}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}"#,
        )
        .unwrap();
        assert_eq!(
            completion.choices[0].message.content.as_deref(),
            Some("Hello there")
        );
        assert_eq!(completion.usage.as_ref().unwrap().total_tokens, Some(5));
    }

    #[test]
    fn parses_error_body_and_is_lenient_about_shape() {
        let body: WireErrorBody =
            serde_json::from_str(r#"{"error":{"message":"bad key","code":401,"type":"auth"}}"#)
                .unwrap();
        assert_eq!(body.error.unwrap().message.as_deref(), Some("bad key"));

        // Non-object bodies do not parse; `provider_error_event` then falls
        // back to the raw (truncated) body text for the message.
        assert!(serde_json::from_str::<WireErrorBody>("\"gateway timeout\"").is_err());
    }

    #[test]
    fn parses_model_list() {
        let list: WireModelList =
            serde_json::from_str(r#"{"data":[{"id":"a","object":"model"},{"id":"b"}]}"#).unwrap();
        let ids: Vec<String> = list.data.into_iter().filter_map(|m| m.id).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn parses_model_reasoning_metadata() {
        let list: WireModelList = serde_json::from_str(
            r#"{"data":[{"id":"deepseek-v4.1-flash","reasoning":{"effort_levels":[{"value":"low","display":"Low"},{"value":"high","display":"High"}],"default_effort_level":"high"}}]}"#,
        )
        .unwrap();
        let reasoning = list.data[0].reasoning.as_ref().unwrap();
        assert_eq!(
            reasoning
                .effort_levels
                .iter()
                .filter_map(|l| l.value.clone())
                .collect::<Vec<_>>(),
            vec!["low", "high"]
        );
        assert_eq!(reasoning.default_effort_level.as_deref(), Some("high"));
    }

    #[test]
    fn streaming_request_carries_reasoning_effort() {
        let request = ChatRequest::new("m", vec![ChatMessage::user("hi")])
            .with_reasoning_effort(Some("high".into()));
        let json = serde_json::to_value(WireChatRequest::streaming(&request)).unwrap();
        assert_eq!(json["reasoning_effort"], "high");

        // Blank effort is dropped, not sent.
        let request = ChatRequest::new("m", vec![ChatMessage::user("hi")])
            .with_reasoning_effort(Some("  ".into()));
        let json = serde_json::to_value(WireChatRequest::streaming(&request)).unwrap();
        assert!(json.get("reasoning_effort").is_none());
    }

    #[test]
    fn parses_reasoning_delta_chunk() {
        let chunk: WireChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"reasoning_content":"We need"}}]}"#)
                .unwrap();
        assert_eq!(chunk.choices[0].delta.reasoning_text(), Some("We need"));
        assert_eq!(chunk.choices[0].delta.content, None);

        // OpenRouter's other spelling is accepted too.
        let chunk: WireChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"reasoning":"hmm"}}]}"#).unwrap();
        assert_eq!(chunk.choices[0].delta.reasoning_text(), Some("hmm"));
    }

    #[test]
    fn reasoning_is_not_sent_back_in_messages() {
        let request = ChatRequest::new(
            "m",
            vec![ChatMessage::assistant("42").with_reasoning("17*23")],
        );
        let json = serde_json::to_value(WireChatRequest::streaming(&request)).unwrap();
        assert_eq!(
            json["messages"],
            serde_json::json!([{"role": "assistant", "content": "42"}])
        );
    }

    #[test]
    fn roles_serialize_lowercase() {
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
    }
}
