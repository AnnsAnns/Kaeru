mod agent_loop;
mod registry;

use super::*;
use crate::audit::AuditLog;
use crate::events::{ApprovalKind, Risk, Usage};
use crate::llm::{
    Cassette, ChatFuture, ChatRequest, ClientMode, FakeProvider, Interaction, LlmClient,
    ModelsFuture, Role,
};
use crate::sandbox::Sandbox;
use crate::search::{FakeSearch, SearchResult};
use crate::tools::{
    MemoryStore, MemoryWriteTool, PythonTool, Tool, ToolContext, ToolFuture, ToolRegistry,
};
use serde_json::{Value, json};
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::mpsc;

fn core_with(fake: FakeProvider) -> Arc<AgentCore> {
    Arc::new(AgentCore::new(
        crate::config::Config::default(),
        std::sync::Arc::new(fake),
    ))
}

fn cassette_for(request: &ChatRequest, events: Vec<CoreEvent>) -> Cassette {
    Cassette {
        cassette_version: 1,
        recorded_at_unix: None,
        base_url: None,
        models: vec![],
        interactions: vec![Interaction {
            request: request.clone(),
            events,
        }],
    }
}

async fn drain(mut rx: EventStream) -> Vec<CoreEvent> {
    let mut events = Vec::new();
    loop {
        match rx.recv().await {
            Ok(event) => events.push(event),
            Err(broadcast::error::RecvError::Closed) => return events,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                panic!("unexpected lag of {n} events in a unit test stream")
            }
        }
    }
}

fn quick_turn(events: Vec<CoreEvent>) -> (Arc<AgentCore>, ChatRequest) {
    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    );
    (
        core_with(FakeProvider::from_cassette(cassette_for(&request, events))),
        request,
    )
}

/// A scripted client: returns queued event batches in call order and
/// records every request it saw (so tests can inspect the payloads).
struct ScriptedClient {
    script: StdMutex<std::collections::VecDeque<Vec<CoreEvent>>>,
    requests: StdMutex<Vec<ChatRequest>>,
}

impl ScriptedClient {
    fn new(script: Vec<Vec<CoreEvent>>) -> Arc<Self> {
        Arc::new(Self {
            script: StdMutex::new(script.into()),
            requests: StdMutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl LlmClient for ScriptedClient {
    fn chat(&self, request: ChatRequest) -> ChatFuture {
        self.requests.lock().unwrap().push(request);
        let events = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![CoreEvent::TurnDone { usage: None }]);
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(64);
            tokio::spawn(async move {
                for event in events {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
            });
            Ok(rx)
        })
    }

    fn list_models(&self) -> ModelsFuture {
        Box::pin(async { Ok(vec![]) })
    }
}

/// A tool that records its inputs and returns a fixed output; optionally
/// emits one artifact (M5 persistence tests).
struct StubTool {
    name: &'static str,
    risk: Risk,
    output: String,
    seen: Arc<StdMutex<Vec<Value>>>,
    artifact: Option<String>,
}

impl Tool for StubTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        "a test stub"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn risk(&self, _input: &Value) -> Risk {
        self.risk.clone()
    }
    fn execute(&self, input: Value, ctx: ToolContext) -> ToolFuture {
        self.seen.lock().unwrap().push(input);
        let output = self.output.clone();
        let artifact = self.artifact.clone();
        Box::pin(async move {
            if let (Some(path), Some(sink)) = (artifact, ctx.artifacts) {
                sink.emit(&path, Some("image/png"));
            }
            Ok(output)
        })
    }
}

fn core_scripted(
    client: Arc<dyn LlmClient>,
    tools: ToolRegistry,
    audit: AuditLog,
) -> Arc<AgentCore> {
    Arc::new(
        AgentCore::with_mode(crate::config::Config::default(), client, ClientMode::Live)
            .with_tools(tools)
            .with_audit(audit),
    )
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kaeru-m3-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn happy_path_streams_and_records_history() {
    let (core, request) = quick_turn(vec![
        CoreEvent::Delta { text: "Hel".into() },
        CoreEvent::Delta { text: "lo".into() },
        CoreEvent::TurnDone {
            usage: Some(Usage {
                input_tokens: Some(1),
                output_tokens: Some(2),
                total_tokens: None,
            }),
        },
    ]);
    let session = ChatSession::new(core, "test");

    let handle = session.send("hello").unwrap();
    let events = drain(handle.into_events()).await;
    assert_eq!(
        events,
        vec![
            CoreEvent::Delta { text: "Hel".into() },
            CoreEvent::Delta { text: "lo".into() },
            CoreEvent::TurnDone {
                usage: Some(Usage {
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    total_tokens: None
                })
            },
        ]
    );

    let history = session.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0], ChatMessage::user("hello"));
    assert_eq!(history[1], ChatMessage::assistant("Hello"));
    assert!(!session.is_active());
    // The request the provider saw must contain exactly this history.
    assert_eq!(request.messages.len(), 1);
}

#[tokio::test]
async fn second_turn_request_carries_conversation_history() {
    let first = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    );
    let second = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![
            ChatMessage::user("hello"),
            ChatMessage::assistant("Hello"),
            ChatMessage::user("again"),
        ],
    );
    let cassette = Cassette {
        interactions: vec![
            Interaction {
                request: first,
                events: vec![
                    CoreEvent::Delta {
                        text: "Hello".into(),
                    },
                    CoreEvent::TurnDone { usage: None },
                ],
            },
            Interaction {
                request: second,
                events: vec![
                    CoreEvent::Delta {
                        text: "Again".into(),
                    },
                    CoreEvent::TurnDone { usage: None },
                ],
            },
        ],
        ..cassette_for(&ChatRequest::new("", vec![]), vec![])
    };
    let session = ChatSession::new(core_with(FakeProvider::from_cassette(cassette)), "test");

    let h1 = session.send("hello").unwrap();
    drain(h1.into_events()).await;
    let h2 = session.send("again").unwrap();
    let events = drain(h2.into_events()).await;
    assert_eq!(
        events,
        vec![
            CoreEvent::Delta {
                text: "Again".into()
            },
            CoreEvent::TurnDone { usage: None },
        ]
    );
    assert_eq!(session.history().len(), 4);
}

#[tokio::test]
async fn busy_turn_is_rejected() {
    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    );
    let session = ChatSession::new(
        core_with(
            FakeProvider::from_cassette(cassette_for(
                &request,
                vec![
                    CoreEvent::Delta { text: "s".into() },
                    CoreEvent::TurnDone { usage: None },
                ],
            ))
            .with_delay(Duration::from_millis(30)),
        ),
        "test",
    );

    let handle = session.send("hello").unwrap();
    let err = session.send("hello").unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::Busy);
    drain(handle.into_events()).await;
    assert!(!session.is_active());
}

#[tokio::test]
async fn abort_stops_the_turn_and_keeps_partial_text() {
    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    );
    let mut events = Vec::new();
    for i in 0..20 {
        events.push(CoreEvent::Delta {
            text: format!("chunk{i} "),
        });
    }
    events.push(CoreEvent::TurnDone { usage: None });
    let fake = FakeProvider::from_cassette(cassette_for(&request, events))
        .with_delay(Duration::from_millis(25));
    let session = ChatSession::new(core_with(fake), "test");

    let handle = session.send("hello").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(session.abort().unwrap());
    let received = drain(handle.into_events()).await;

    assert!(received.iter().any(|e| matches!(
        e,
        CoreEvent::Error {
            kind: ApiErrorKind::Aborted,
            ..
        }
    )));
    let history = session.history();
    assert_eq!(history[0], ChatMessage::user("hello"));
    let partial = history[1].content.clone();
    assert!(
        partial.contains("chunk0"),
        "partial answer must be kept, got: {partial:?}"
    );
    assert!(
        partial.len() < "chunk0 ".len() * 20,
        "partial must be truncated, got: {partial:?}"
    );
    assert!(!session.is_active());
}

#[tokio::test]
async fn provider_failure_with_no_output_pops_the_user_message_for_a_clean_retry() {
    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    );
    let session = ChatSession::new(
        core_with(FakeProvider::from_cassette(cassette_for(
            &request,
            vec![CoreEvent::error(ApiErrorKind::Unauthorized, "bad key")],
        ))),
        "test",
    );

    let handle = session.send("hello").unwrap();
    let received = drain(handle.into_events()).await;
    assert!(matches!(
        received[0],
        CoreEvent::Error {
            kind: ApiErrorKind::Unauthorized,
            ..
        }
    ));
    assert!(
        session.history().is_empty(),
        "failed turn with no output must leave history clean"
    );
}

#[tokio::test]
async fn provider_failure_after_partial_output_keeps_the_partial() {
    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    );
    let session = ChatSession::new(
        core_with(FakeProvider::from_cassette(cassette_for(
            &request,
            vec![
                CoreEvent::Delta {
                    text: "partial answer".into(),
                },
                CoreEvent::error(ApiErrorKind::Provider, "provider exploded mid-stream"),
            ],
        ))),
        "test",
    );

    let handle = session.send("hello").unwrap();
    drain(handle.into_events()).await;
    let history = session.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[1], ChatMessage::assistant("partial answer"));
}

#[tokio::test]
async fn dropped_subscriber_no_longer_aborts_the_turn() {
    // M3 (ADR-015): the turn executor survives a frontend disconnect; a
    // reconnect would replay the buffer. The full answer is persisted.
    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    );
    let mut events = Vec::new();
    for i in 0..10 {
        events.push(CoreEvent::Delta {
            text: format!("d{i} "),
        });
    }
    events.push(CoreEvent::TurnDone { usage: None });
    let fake = FakeProvider::from_cassette(cassette_for(&request, events))
        .with_delay(Duration::from_millis(20));
    let session = ChatSession::new(core_with(fake), "test");

    let handle = session.send("hello").unwrap();
    drop(handle); // frontend disconnect: the turn keeps running (M3)

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if session.history().len() == 2 || tokio::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let history = session.history();
    assert_eq!(history.len(), 2, "user message + full answer must be kept");
    // The whole answer arrived even with nobody listening.
    assert!(history[1].content.contains("d0"));
    assert!(history[1].content.contains("d9"));
    assert!(!session.is_active());
}

#[tokio::test]
async fn subscribe_replays_the_turn_buffer_for_a_reconnect() {
    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    );
    let mut events = Vec::new();
    for i in 0..6 {
        events.push(CoreEvent::Delta {
            text: format!("d{i} "),
        });
    }
    events.push(CoreEvent::TurnDone { usage: None });
    let fake = FakeProvider::from_cassette(cassette_for(&request, events))
        .with_delay(Duration::from_millis(20));
    let session = ChatSession::new(core_with(fake), "test");

    let handle = session.send("hello").unwrap();
    // Let a couple of deltas land in the buffer, then "reconnect".
    tokio::time::sleep(Duration::from_millis(45)).await;
    let mut reconnected = session.subscribe();
    let mut replayed = Vec::new();
    while let Ok(event) = reconnected.recv().await {
        let terminal = matches!(event, CoreEvent::TurnDone { .. });
        replayed.push(event);
        if terminal {
            break;
        }
    }
    // The replay starts from the beginning of the turn, not the reconnect.
    assert!(matches!(replayed[0], CoreEvent::Delta { ref text } if text.contains("d0")));
    assert!(matches!(replayed.last(), Some(CoreEvent::TurnDone { .. })));
    drain(handle.into_events()).await;
}

#[tokio::test]
async fn subscribe_taps_the_active_turn_and_closes_after_it_ends() {
    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    );
    let fake = FakeProvider::from_cassette(cassette_for(
        &request,
        vec![
            CoreEvent::Delta { text: "x".into() },
            CoreEvent::TurnDone { usage: None },
        ],
    ))
    .with_delay(Duration::from_millis(40));
    let session = ChatSession::new(core_with(fake), "test");

    let handle = session.send("hello").unwrap();
    let mut tap = session.subscribe();
    let tapped = tokio::time::timeout(Duration::from_secs(2), tap.recv()).await;
    assert!(tapped.is_ok(), "live subscriber must receive events");
    drain(handle.into_events()).await;

    let mut idle = session.subscribe();
    match idle.recv().await {
        Err(broadcast::error::RecvError::Closed) => {}
        other => panic!("idle subscribe must be a closed stream, got {other:?}"),
    }
}

#[tokio::test]
async fn model_override_reaches_the_provider_request() {
    let request = ChatRequest::new("custom/model", vec![ChatMessage::user("hello")]);
    let session = ChatSession::new(
        core_with(FakeProvider::from_cassette(cassette_for(
            &request,
            vec![CoreEvent::TurnDone { usage: None }],
        ))),
        "test",
    );
    session.set_model(Some("custom/model".into()));
    assert_eq!(session.current_model(), "custom/model");
    let handle = session.send("hello").unwrap();
    drain(handle.into_events()).await;
    session.set_model(None);
    assert_eq!(session.current_model(), crate::config::DEFAULT_MODEL);
}

#[tokio::test]
async fn reasoning_streams_through_and_is_kept_out_of_the_answer() {
    let (core, _request) = quick_turn(vec![
        CoreEvent::Reasoning {
            text: "let me think".into(),
        },
        CoreEvent::Delta {
            text: "answer".into(),
        },
        CoreEvent::TurnDone { usage: None },
    ]);
    let session = ChatSession::new(core, "test");

    let handle = session.send("hello").unwrap();
    let events = drain(handle.into_events()).await;
    assert_eq!(
        events,
        vec![
            CoreEvent::Reasoning {
                text: "let me think".into()
            },
            CoreEvent::Delta {
                text: "answer".into()
            },
            CoreEvent::TurnDone { usage: None },
        ]
    );

    // Thinking is stored alongside the assistant message (for display on
    // reload) but is never folded into its content.
    let history = session.history();
    assert_eq!(history[0].reasoning, None);
    assert_eq!(history[1].content, "answer");
    assert_eq!(history[1].reasoning.as_deref(), Some("let me think"));
}

#[tokio::test]
async fn reasoning_effort_override_reaches_the_provider_request() {
    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user("hello")],
    )
    .with_reasoning_effort(Some("high".into()));
    let session = ChatSession::new(
        core_with(FakeProvider::from_cassette(cassette_for(
            &request,
            vec![CoreEvent::TurnDone { usage: None }],
        ))),
        "test",
    );
    session.set_reasoning_effort(Some("high".into()));
    assert_eq!(session.current_reasoning_effort(), Some("high".into()));
    // A mismatch with the cassette would surface as an Error event, so
    // reaching TurnDone proves the effort rode along on the request.
    let handle = session.send("hello").unwrap();
    let events = drain(handle.into_events()).await;
    assert_eq!(events, vec![CoreEvent::TurnDone { usage: None }]);
    session.set_reasoning_effort(None);
    assert_eq!(session.current_reasoning_effort(), None);
}

#[test]
fn empty_message_is_rejected() {
    let session = ChatSession::new(core_with(FakeProvider::builtin()), "test");
    let err = session.send("   ").unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::Config);
}

#[tokio::test]
async fn approving_an_unknown_request_is_not_found() {
    // M3: the consent flow is implemented; an id nobody is waiting on is a
    // plain NotFound (already resolved or timed out).
    let session = ChatSession::new(core_with(FakeProvider::builtin()), "test");
    let err = session.approve("appr_1", Decision::Allow).unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::NotFound);
    let _ = ApprovalKind::PackageInstall { packages: vec![] }; // M5 consent type
}

#[test]
fn turn_handle_abort_is_id_guarded() {
    let session = ChatSession::new(core_with(FakeProvider::builtin()), "test");
    // Idle session: aborting a stale handle is a no-op.
    let fake_handle_session = Arc::new(Mutex::new(SessionInner {
        conversation_id: "test".into(),
        history: vec![],
        summary: None,
        accumulated_usage: Usage::default(),
        title: None,
        created_at: "1970-01-01T00:00:00Z".into(),
        updated_at: "1970-01-01T00:00:00Z".into(),
        model_override: None,
        reasoning_effort_override: None,
        next_turn_id: 1,
        active: None,
        store: None,
    }));
    assert!(!abort_turn(&fake_handle_session, Some(99)).unwrap());
    assert!(!session.abort().unwrap());
}

#[tokio::test]
async fn over_budget_turn_compacts_oldest_turns_into_a_summary_and_answers() {
    // Budget 4 estimated tokens: the ("hello", "Hello!") pair falls out of
    // the window together (user-boundary alignment); only the new
    // message stays and the pair is folded into the rolling summary.
    let mut config = crate::config::Config::default();
    config.context.max_prompt_tokens = 4;
    let model = crate::config::DEFAULT_MODEL;

    let first_request = ChatRequest::new(model, vec![ChatMessage::user("hello")]);
    let dropped = vec![ChatMessage::user("hello"), ChatMessage::assistant("Hello!")];
    let summary_request = crate::context::summary_request(model, None, &dropped);
    let second_window = vec![ChatMessage::user("again")];
    let second_request = ChatRequest::new(
        model,
        crate::context::assemble(None, None, Some("compact summary"), &second_window),
    );

    let cassette = Cassette {
        interactions: vec![
            Interaction {
                request: first_request,
                events: vec![
                    CoreEvent::Delta {
                        text: "Hello!".into(),
                    },
                    CoreEvent::TurnDone {
                        usage: Some(Usage {
                            input_tokens: Some(1),
                            output_tokens: Some(2),
                            total_tokens: None,
                        }),
                    },
                ],
            },
            Interaction {
                request: summary_request,
                events: vec![
                    CoreEvent::Delta {
                        text: "compact summary".into(),
                    },
                    CoreEvent::TurnDone {
                        usage: Some(Usage {
                            input_tokens: Some(10),
                            output_tokens: Some(5),
                            total_tokens: None,
                        }),
                    },
                ],
            },
            Interaction {
                request: second_request,
                events: vec![
                    CoreEvent::Delta {
                        text: "Answer 2".into(),
                    },
                    CoreEvent::TurnDone {
                        usage: Some(Usage {
                            input_tokens: Some(3),
                            output_tokens: Some(4),
                            total_tokens: None,
                        }),
                    },
                ],
            },
        ],
        ..cassette_for(&ChatRequest::new("", vec![]), vec![])
    };
    let session = ChatSession::new(
        Arc::new(AgentCore::new(
            config,
            std::sync::Arc::new(FakeProvider::from_cassette(cassette)),
        )),
        "test",
    );

    assert_eq!(session.summary(), None);
    let h1 = session.send("hello").unwrap();
    let received = drain(h1.into_events()).await;
    assert!(
        received
            .last()
            .is_some_and(|e| matches!(e, CoreEvent::TurnDone { .. }))
    );
    assert_eq!(session.total_usage().input_tokens, Some(1));

    let h2 = session.send("again").unwrap();
    let received = drain(h2.into_events()).await;
    // The over-budget turn still answers (Appendix C, M2).
    assert!(received.contains(&CoreEvent::Delta {
        text: "Answer 2".into()
    }));
    assert!(
        received
            .iter()
            .any(|e| matches!(e, CoreEvent::TurnDone { .. }))
    );

    // Drop-oldest + summary: history is truncated, summary is stored.
    assert_eq!(session.summary(), Some("compact summary".into()));
    assert_eq!(
        session.history(),
        vec![
            ChatMessage::user("again"),
            ChatMessage::assistant("Answer 2"),
        ]
    );
    // Usage accounting covers the main turns and the summary sub-call.
    assert_eq!(session.total_usage().input_tokens, Some(1 + 10 + 3));
    assert_eq!(session.total_usage().output_tokens, Some(2 + 5 + 4));
}

#[tokio::test]
async fn persisted_conversation_reloads_after_restart() {
    let dir = std::env::temp_dir().join(format!("kaeru-test-{}-persist", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let store = ConversationStore::new(&dir);

    let request = ChatRequest::new(
        crate::config::DEFAULT_MODEL,
        vec![ChatMessage::user(
            "a longer first question, good for a title",
        )],
    );
    let cassette = cassette_for(
        &request,
        vec![
            CoreEvent::Delta {
                text: "Hello!".into(),
            },
            CoreEvent::TurnDone {
                usage: Some(Usage {
                    input_tokens: Some(4),
                    output_tokens: Some(6),
                    total_tokens: Some(10),
                }),
            },
        ],
    );

    // "Process 1": chat, then shut down.
    {
        let session = ChatSession::with_store(
            core_with(FakeProvider::from_cassette(cassette.clone())),
            "default",
            store.clone(),
        );
        assert!(session.history().is_empty());
        let handle = session
            .send("a longer first question, good for a title")
            .unwrap();
        drain(handle.into_events()).await;
        assert_eq!(session.history().len(), 2);
    }

    // "Process 2": same data dir — reload restores history, usage, title.
    let session = ChatSession::with_store(
        core_with(FakeProvider::from_cassette(cassette)),
        "default",
        store,
    );
    assert_eq!(
        session.conversation_id(),
        "default",
        "conversation id must survive restarts"
    );
    let history = session.history();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[0],
        ChatMessage::user("a longer first question, good for a title")
    );
    assert_eq!(history[1], ChatMessage::assistant("Hello!"));
    assert_eq!(
        session.title().as_deref(),
        Some("a longer first question, good for a title")
    );
    assert_eq!(session.total_usage().input_tokens, Some(4));
    assert_eq!(session.total_usage().output_tokens, Some(6));

    // The file on disk carries the planned schema (Appendix C: M2.5 added
    // `updatedAt` in v2, M5 added message `artifacts` in v3).
    let text = std::fs::read_to_string(dir.join("default.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["schema"], 3);
    assert!(json["updatedAt"].is_string());
    assert_eq!(json["messages"].as_array().unwrap().len(), 2);
    assert_eq!(json["usage"]["total_tokens"], 10);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn attachments_and_artifacts_persist_with_their_messages() {
    let dir = std::env::temp_dir().join(format!("kaeru-test-{}-files", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let store = ConversationStore::new(&dir);

    let seen = Arc::new(StdMutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    registry.register(StubTool {
        name: "echo",
        risk: Risk::Safe,
        output: "OK".into(),
        seen: Arc::clone(&seen),
        artifact: Some("rotated.png".into()),
    });
    let client = ScriptedClient::new(vec![
        vec![
            CoreEvent::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: json!({}),
            },
            CoreEvent::TurnDone { usage: None },
        ],
        vec![
            CoreEvent::Delta {
                text: "rotated it".into(),
            },
            CoreEvent::TurnDone { usage: None },
        ],
    ]);
    let session = ChatSession::with_store(
        core_scripted(client, registry, AuditLog::disabled()),
        "default",
        store.clone(),
    );
    let handle = session
        .send_with_attachments(
            "rotate this",
            vec![Artifact::new("photo.png", Some("image/png"))],
        )
        .unwrap();
    let events = drain(handle.into_events()).await;
    assert!(
        events.iter().any(
            |event| matches!(event, CoreEvent::Artifact { path, .. } if path == "rotated.png")
        )
    );

    // The upload rides on the user message; the tool output lands on the
    // final answer (the intermediate message only carries the tool call).
    let history = session.history();
    assert_eq!(
        history[0].artifacts,
        vec![Artifact::new("photo.png", Some("image/png"))]
    );
    assert_eq!(
        history.last().unwrap().artifacts,
        vec![Artifact::new("rotated.png", Some("image/png"))]
    );
    assert!(
        history[1].artifacts.is_empty(),
        "tool-call turn: no artifacts"
    );

    // Reload from disk: both survive, schema v3.
    let reloaded = ChatSession::with_store(
        core_scripted(
            ScriptedClient::new(Vec::new()),
            ToolRegistry::new(),
            AuditLog::disabled(),
        ),
        "default",
        store,
    );
    let history = reloaded.history();
    assert_eq!(history[0].artifacts[0].path, "photo.png");
    assert_eq!(history.last().unwrap().artifacts[0].path, "rotated.png");
    let text = std::fs::read_to_string(dir.join("default.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["schema"], 3);
    assert_eq!(json["messages"][0]["artifacts"][0]["path"], "photo.png");
    assert_eq!(json["messages"][0]["artifacts"][0]["mimeHint"], "image/png");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn regenerate_resends_the_original_attachments() {
    let client = ScriptedClient::new(vec![
        vec![
            CoreEvent::Delta {
                text: "first".into(),
            },
            CoreEvent::TurnDone { usage: None },
        ],
        vec![
            CoreEvent::Delta {
                text: "second".into(),
            },
            CoreEvent::TurnDone { usage: None },
        ],
    ]);
    let core = core_scripted(client, ToolRegistry::new(), AuditLog::disabled());
    let session = ChatSession::new(core, "test");
    drain(
        session
            .send_with_attachments("look", vec![Artifact::new("photo.png", None)])
            .unwrap()
            .into_events(),
    )
    .await;
    drain(session.regenerate().unwrap().into_events()).await;
    let history = session.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].artifacts[0].path, "photo.png");
    assert_eq!(history[1].content, "second");
}

#[tokio::test]
async fn an_unreadable_conversation_file_starts_empty_not_broken() {
    let dir = std::env::temp_dir().join(format!("kaeru-test-{}-broken", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("default.json"), "}{ broken").unwrap();

    let session = ChatSession::with_store(
        core_with(FakeProvider::builtin()),
        "default",
        ConversationStore::new(&dir),
    );
    assert!(session.history().is_empty());
    assert_eq!(session.conversation_id(), "default");
    assert!(dir.join("default.json.quarantine").is_file());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn titles_come_from_the_first_user_message() {
    assert_eq!(derive_title("hello"), "hello");
    assert_eq!(derive_title("first line\nsecond line"), "first line");
    let long = "x".repeat(100);
    let title = derive_title(&long);
    assert_eq!(title.chars().count(), 61);
    assert!(title.ends_with('…'));
    assert_eq!(derive_title("   "), "untitled");
}
