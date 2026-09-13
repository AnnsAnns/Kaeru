use super::*;

#[test]
fn delta_payload_becomes_delta_event() {
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();
    let events = events_from_payload(
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"}}]}"#,
        &mut usage,
        &mut tool_calls,
    );
    assert_eq!(events, vec![CoreEvent::Delta { text: "Hel".into() }]);
    assert!(usage.is_none());
}

#[test]
fn usage_payload_is_captured_not_emitted() {
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();
    let events = events_from_payload(
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
        &mut usage,
        &mut tool_calls,
    );
    assert!(events.is_empty());
    assert_eq!(
        usage,
        Some(Usage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
        })
    );
}

#[test]
fn tool_call_fragments_accumulate_into_complete_calls() {
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();
    for payload in [
        r#"{"choices":[{"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"web_search","arguments":""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"query\":"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"frogs\"}"}}]}}]}"#,
    ] {
        assert!(events_from_payload(payload, &mut usage, &mut tool_calls).is_empty());
    }
    assert_eq!(
        tool_calls.take_events(),
        vec![CoreEvent::ToolCall {
            id: "call_1".into(),
            name: "web_search".into(),
            input: serde_json::json!({"query": "frogs"}),
        }]
    );
}

#[test]
fn absurd_tool_call_indexes_are_dropped_not_allocated() {
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();
    let payload = format!(
        r#"{{"choices":[{{"delta":{{"tool_calls":[{{"index":{},"id":"call_x","function":{{"name":"web_search","arguments":"{{}}"}}}}]}}}}]}}"#,
        usize::MAX
    );
    assert!(events_from_payload(&payload, &mut usage, &mut tool_calls).is_empty());
    assert!(
        tool_calls.calls.is_empty(),
        "no slot may be allocated for an out-of-range index"
    );
    assert!(tool_calls.take_events().is_empty());
}

#[test]
fn empty_content_deltas_are_dropped() {
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();
    let events = events_from_payload(
        r#"{"choices":[{"delta":{"content":""}}]}"#,
        &mut usage,
        &mut tool_calls,
    );
    assert!(events.is_empty());
}

#[test]
fn malformed_payloads_are_skipped() {
    let mut usage = None;
    let mut tool_calls = ToolCallAccumulator::default();
    assert!(events_from_payload("not json at all", &mut usage, &mut tool_calls).is_empty());
    assert!(events_from_payload(r#"{"unexpected": true}"#, &mut usage, &mut tool_calls).is_empty());
    assert!(usage.is_none());
}

#[test]
fn error_status_maps_to_kinds() {
    let CoreEvent::Error { kind, message } =
        provider_error_event(401, r#"{"error":{"message":"invalid key"}}"#)
    else {
        panic!("expected error event");
    };
    assert_eq!(kind, ApiErrorKind::Unauthorized);
    assert!(message.contains("invalid key"));

    let CoreEvent::Error { kind, .. } = provider_error_event(429, "") else {
        panic!("expected error event");
    };
    assert_eq!(kind, ApiErrorKind::RateLimited);

    let CoreEvent::Error { kind, message } = provider_error_event(500, "boom") else {
        panic!("expected error event");
    };
    assert_eq!(kind, ApiErrorKind::Provider);
    assert!(message.contains("boom"), "expected body text in: {message}");
}

#[test]
fn error_message_falls_back_to_truncated_body() {
    let long = "x".repeat(500);
    let CoreEvent::Error { message, .. } = provider_error_event(502, &long) else {
        panic!("expected error event");
    };
    assert!(
        message.contains('…'),
        "expected truncation marker in: {message}"
    );
}

#[tokio::test]
async fn forward_payload_done_completes_turn_with_captured_usage() {
    let (tx, mut rx) = mpsc::channel(CLIENT_EVENT_CAPACITY);
    let mut usage = Some(Usage {
        input_tokens: Some(1),
        output_tokens: Some(2),
        total_tokens: None,
    });
    let mut done = false;
    let mut tool_calls = ToolCallAccumulator::default();
    assert!(forward_payload(&tx, &mut usage, &mut tool_calls, &mut done, "[DONE]".into()).await);
    assert!(done);
    drop(tx);
    let events: Vec<CoreEvent> = rx.recv().await.into_iter().collect();
    assert_eq!(
        events,
        vec![CoreEvent::TurnDone {
            usage: Some(Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: None,
            })
        }]
    );
}

#[tokio::test]
async fn forward_payload_stops_when_consumer_is_gone() {
    let (tx, rx) = mpsc::channel(CLIENT_EVENT_CAPACITY);
    drop(rx);
    let mut usage = None;
    let mut done = false;
    let mut tool_calls = ToolCallAccumulator::default();
    assert!(!forward_payload(&tx, &mut usage, &mut tool_calls, &mut done, "[DONE]".into()).await);
}
