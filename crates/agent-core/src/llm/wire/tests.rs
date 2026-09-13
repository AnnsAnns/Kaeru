use super::*;
use crate::events::Usage;
use crate::llm::types::{ChatMessage, ChatRequest, ToolCall};

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
