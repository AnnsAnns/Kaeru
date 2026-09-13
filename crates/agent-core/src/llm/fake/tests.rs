use super::*;
use crate::llm::types::ChatMessage;

fn sample_cassette() -> Cassette {
    Cassette {
        cassette_version: 1,
        recorded_at_unix: Some(1_760_000_000),
        base_url: Some("https://example.test/v1".into()),
        models: vec![ModelInfo::new("m/test")],
        interactions: vec![Interaction {
            request: ChatRequest::new("m/test", vec![ChatMessage::user("hello")]),
            events: vec![
                CoreEvent::Delta { text: "Hel".into() },
                CoreEvent::Delta { text: "lo".into() },
                CoreEvent::TurnDone { usage: None },
            ],
        }],
    }
}

fn temp_cassette_path(name: &str) -> PathBuf {
    crate::util::temp_dir("fake", name).join("cassette.json")
}

async fn collect(rx: mpsc::Receiver<CoreEvent>) -> Vec<CoreEvent> {
    let mut events = Vec::new();
    let mut rx = rx;
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

#[test]
fn cassette_round_trips_through_json() {
    let path = temp_cassette_path("roundtrip");
    let cassette = sample_cassette();
    cassette.save(&path).unwrap();
    let loaded = Cassette::load(&path).unwrap();
    assert_eq!(loaded, cassette);
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn cassette_version_mismatch_is_a_clear_error() {
    let path = temp_cassette_path("version");
    std::fs::write(&path, r#"{"cassette_version": 99, "interactions": []}"#).unwrap();
    let err = Cassette::load(&path).unwrap_err();
    assert!(err.message.contains("re-record"));
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[tokio::test]
async fn replays_matching_request_and_loops_on_replay() {
    let fake = FakeProvider::from_cassette(sample_cassette());
    let request = ChatRequest::new("m/test", vec![ChatMessage::user("hello")]);
    for _ in 0..2 {
        let events = collect(fake.chat(request.clone()).await.unwrap()).await;
        assert_eq!(
            events,
            vec![
                CoreEvent::Delta { text: "Hel".into() },
                CoreEvent::Delta { text: "lo".into() },
                CoreEvent::TurnDone { usage: None },
            ]
        );
    }
}

#[tokio::test]
async fn unmatched_request_without_fallback_is_a_loud_error() {
    let fake = FakeProvider::from_cassette(sample_cassette());
    let request = ChatRequest::new("m/test", vec![ChatMessage::user("never recorded")]);
    let events = collect(fake.chat(request).await.unwrap()).await;
    let CoreEvent::Error { kind, message } = &events[0] else {
        panic!("expected error event, got {events:?}");
    };
    assert_eq!(*kind, ApiErrorKind::Internal);
    assert!(message.contains("re-record"));
}

#[tokio::test]
async fn builtin_answers_any_request_keyless() {
    let fake = FakeProvider::builtin();
    let request = ChatRequest::new("whatever", vec![ChatMessage::user("anything")]);
    let events = collect(fake.chat(request).await.unwrap()).await;
    assert!(matches!(events.last(), Some(CoreEvent::TurnDone { .. })));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::Delta { text } if text.contains("fake provider")))
    );
    let models = fake.list_models().await.unwrap();
    assert_eq!(models[0].id, "openai/gpt-4o-mini");
}

#[tokio::test]
async fn recording_client_records_and_the_cassette_replays() {
    let path = temp_cassette_path("record");
    let inner: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
    let recorder = RecordingClient::new(Arc::clone(&inner), &path);

    let request = ChatRequest::new("openai/gpt-4o-mini", vec![ChatMessage::user("hi there")]);
    let events = collect(recorder.chat(request.clone()).await.unwrap()).await;
    assert!(!events.is_empty());

    let models = recorder.list_models().await.unwrap();
    assert!(!models.is_empty());

    let cassette = Cassette::load(&path).unwrap();
    assert_eq!(cassette.models.len(), models.len());
    assert_eq!(cassette.interactions.len(), 1);
    assert_eq!(cassette.interactions[0].request, request);
    assert_eq!(cassette.interactions[0].events, events);

    let replay = FakeProvider::from_cassette(cassette);
    let replayed = collect(replay.chat(request).await.unwrap()).await;
    assert_eq!(replayed, events);
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[tokio::test]
async fn concurrent_turns_all_reach_the_cassette() {
    let path = temp_cassette_path("record-concurrent");
    let inner: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
    let recorder = Arc::new(RecordingClient::new(inner, &path));

    let mut handles = Vec::new();
    for i in 0..8 {
        let recorder = Arc::clone(&recorder);
        handles.push(tokio::spawn(async move {
            let request = ChatRequest::new("m", vec![ChatMessage::user(format!("hi {i}"))]);
            collect(recorder.chat(request).await.unwrap()).await;
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let cassette = Cassette::load(&path).unwrap();
    assert_eq!(
        cassette.interactions.len(),
        8,
        "a concurrent recording must not be lost to a write race"
    );
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[tokio::test]
async fn record_models_refreshes_a_stale_cassette() {
    let path = temp_cassette_path("record-models-refresh");
    let mut stale = Cassette::new();
    stale.models = vec![ModelInfo::new("old/model")];
    stale.save(&path).unwrap();

    let inner: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
    let recorder = RecordingClient::new(inner, &path);
    let models = recorder.list_models().await.unwrap();

    let reloaded = Cassette::load(&path).unwrap();
    assert_eq!(reloaded.models, models, "the model list must be refreshed");
    assert!(reloaded.models.iter().any(|m| m.id == "openai/gpt-4o-mini"));
    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}
