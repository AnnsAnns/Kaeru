use super::*;
use crate::agent::workers::REFLECTOR_SYSTEM;
use crate::config::Config;
use crate::conversations::{CONVERSATION_SCHEMA_VERSION, StoredMessage};
use crate::events::{CoreEvent, Usage};
use crate::llm::{Cassette, ChatMessage, ChatRequest, FakeProvider, Interaction, LlmClient};
use std::path::Path;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kaeru-reflect-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn conversation(id: &str, updated_at: &str, messages: Vec<(&str, &str)>) -> Conversation {
    Conversation {
        schema: CONVERSATION_SCHEMA_VERSION,
        id: id.to_owned(),
        title: Some(format!("thread {id}")),
        created_at: "2026-09-01T00:00:00Z".into(),
        updated_at: updated_at.to_owned(),
        summary: None,
        messages: messages
            .into_iter()
            .map(|(role, content)| StoredMessage {
                role: match role {
                    "user" => crate::llm::Role::User,
                    _ => crate::llm::Role::Assistant,
                },
                content: content.to_owned(),
                tool_call_id: None,
                tool_calls: None,
                reasoning: None,
                artifacts: Vec::new(),
            })
            .collect(),
        usage: Usage::default(),
    }
}

/// Persist a conversation with an exact `updatedAt` (the store normally
/// owns the clock, so tests that need a chosen timestamp write directly).
fn save_with_updated(conversations: &ConversationStore, conversation: &Conversation) {
    std::fs::create_dir_all(conversations.dir()).unwrap();
    std::fs::write(
        conversations
            .dir()
            .join(format!("{}.json", conversation.id)),
        serde_json::to_string(conversation).unwrap(),
    )
    .unwrap();
}

/// The content the digest builds for a conversation set (no persona).
fn digest_content_for_store(conversations: &ConversationStore) -> String {
    digest_content_for_store_with_persona(conversations, None)
}

/// The content the digest builds, including the persona prefix when given.
fn digest_content_for_store_with_persona(
    conversations: &ConversationStore,
    persona: Option<&str>,
) -> String {
    let list = conversations.list().unwrap();
    digest_content(&list, persona)
}

fn reflector(
    config: Config,
    client: Arc<dyn LlmClient>,
    dir: &Path,
    conversations: ConversationStore,
) -> Reflector {
    let core = Arc::new(
        AgentCore::with_mode(
            config,
            client,
            crate::llm::ClientMode::Fake {
                cassette: PathBuf::new(),
            },
        )
        .with_memory(MemoryStore::new(dir.join("memory"))),
    );
    Reflector::from_core(core, conversations, dir.join("reflect-state.json")).unwrap()
}

#[test]
fn parse_reflection_splits_notes_and_tags() {
    let text = "rust, axum\n---\nOwnership notes\n===\n---\nA body with no tag line\n";
    let reflection = parse_reflection(text);
    let notes = reflection.notes;
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0].tags, vec!["rust".to_string(), "axum".to_string()]);
    assert_eq!(notes[0].body, "Ownership notes");
    assert!(notes[1].tags.is_empty());
    assert_eq!(notes[1].body, "A body with no tag line");
    assert!(reflection.persona.is_none());
}

#[test]
fn parse_reflection_extracts_an_optional_persona_block() {
    let text = "frogs\n---\nA note\n\
                ===PERSONA===\n\
                WHY: the day showed patience matters\n\
                HOW: added a line about slowing down\n\
                ---\n\
                # Character\n\
                I am a patient pond frog.\n\
                ===END===\n";
    let reflection = parse_reflection(text);
    assert_eq!(reflection.notes.len(), 1);
    let revision = reflection.persona.expect("persona block parsed");
    assert_eq!(revision.why, "the day showed patience matters");
    assert_eq!(revision.how, "added a line about slowing down");
    assert_eq!(revision.persona, "# Character\nI am a patient pond frog.");
}

#[test]
fn parse_reflection_handles_unterminated_and_consideration_only_persona_blocks() {
    // An unterminated block is ignored; regular notes still count.
    let text = "frogs\n---\nA note\n===PERSONA===\nWHY: x\n---\nno end marker\n";
    let reflection = parse_reflection(text);
    assert_eq!(reflection.notes.len(), 1);
    assert!(reflection.persona.is_none());

    // A block with only the reasoning is a reflection: no revision.
    let text = "frogs\n---\nA note\n===PERSONA===\nWHY: x\nHOW: still thinking\n===END===\n";
    let reflection = parse_reflection(text);
    assert_eq!(reflection.notes.len(), 1);
    let revision = reflection.persona.expect("consideration parsed");
    assert_eq!(revision.why, "x");
    assert_eq!(revision.how, "still thinking");
    assert!(revision.persona.is_empty());
}

#[test]
fn reflect_tag_is_added_once() {
    assert_eq!(with_reflect_tag(vec![]), vec![REFLECT_TAG.to_string()]);
    assert_eq!(
        with_reflect_tag(vec!["reflect".into()]),
        vec![REFLECT_TAG.to_string()]
    );
    assert_eq!(
        with_reflect_tag(vec!["frogs".into()]),
        vec!["frogs".to_string(), REFLECT_TAG.to_string()]
    );
}

#[test]
fn scheduling_is_due_when_last_run_precedes_the_cycle() {
    let dir = temp_dir("sched");
    let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
    let reflector = reflector(
        config,
        client,
        &dir,
        ConversationStore::new(dir.join("conversations")),
    );
    // A time far in the future keeps the test timezone-independent.
    let now = 4_102_444_800u64;
    assert!(reflector.due_cycle(now).is_some(), "never run is due");
    reflector.write_state(now).unwrap();
    assert!(reflector.due_cycle(now).is_none(), "same cycle is not due");
    assert!(
        reflector.due_cycle(now + 2 * 86_400).is_some(),
        "next cycle"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn disabled_is_never_due() {
    let dir = temp_dir("disabled");
    let config = Config::parse("[reflect]\nenabled = false\n").unwrap();
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
    let reflector = reflector(
        config,
        client,
        &dir,
        ConversationStore::new(dir.join("conversations")),
    );
    assert!(reflector.due_cycle(1_000_000_000).is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn successful_run_writes_notes_and_advances_state() {
    let dir = temp_dir("success");
    let conversations = ConversationStore::new(dir.join("conversations"));
    conversations
        .save(&conversation(
            "t1",
            "2099-01-01T10:00:00Z",
            vec![("user", "my frog is called Kaeru"), ("assistant", "noted!")],
        ))
        .unwrap();

    let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
    let request = ChatRequest::new(
        config.provider.model.clone(),
        vec![
            ChatMessage::system(REFLECTOR_SYSTEM),
            ChatMessage::user(digest_content_for_store(&conversations)),
        ],
    )
    .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
    let cassette = Cassette {
        cassette_version: crate::llm::CASSETTE_VERSION,
        recorded_at_unix: None,
        base_url: None,
        models: vec![],
        interactions: vec![Interaction {
            request,
            events: vec![
                CoreEvent::Delta {
                    text: "frog, home\n---\nThe user's frog is called Kaeru.\n===\n---\nI should ask how the frog is doing.\n"
                        .into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        }],
    };
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
    let reflector = reflector(config, client, &dir, conversations);

    let outcome = reflector.run_now(2_000_000_000).await.unwrap();
    assert_eq!(outcome.status, ReflectStatus::Ran);
    assert_eq!(outcome.conversations, 1);
    assert_eq!(outcome.notes, 2);

    let notes = reflector.memory.list();
    assert_eq!(notes.len(), 2);
    assert!(
        notes
            .iter()
            .all(|note| note.tags.iter().any(|tag| tag == REFLECT_TAG))
    );
    assert!(
        notes
            .iter()
            .any(|note| note.content.contains("called Kaeru"))
    );
    assert_eq!(reflector.last_run(), 2_000_000_000);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn persona_revision_is_applied_and_recorded_in_memory() {
    let dir = temp_dir("persona-apply");
    let conversations = ConversationStore::new(dir.join("conversations"));
    conversations
        .save(&conversation(
            "t1",
            "2099-01-01T10:00:00Z",
            vec![("user", "hi"), ("assistant", "hello")],
        ))
        .unwrap();
    let persona_path = dir.join("persona.md");
    std::fs::write(&persona_path, "I am a pond frog.\n").unwrap();

    let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
    let request = ChatRequest::new(
        config.provider.model.clone(),
        vec![
            ChatMessage::system(REFLECTOR_SYSTEM),
            ChatMessage::user(digest_content_for_store_with_persona(
                &conversations,
                Some("I am a pond frog."),
            )),
        ],
    )
    .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
    let cassette = Cassette {
        cassette_version: crate::llm::CASSETTE_VERSION,
        recorded_at_unix: None,
        base_url: None,
        models: vec![],
        interactions: vec![Interaction {
            request,
            events: vec![
                CoreEvent::Delta {
                    text: "frogs\n---\nA note\n===PERSONA===\nWHY: it helps\nHOW: added a line about mornings\n---\nI am a pond frog.\nI like quiet mornings.\n===END===\n"
                        .into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        }],
    };
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
    let core = Arc::new(
        AgentCore::with_mode(
            config,
            client,
            crate::llm::ClientMode::Fake {
                cassette: PathBuf::new(),
            },
        )
        .with_persona(persona_path.clone())
        .with_memory(MemoryStore::new(dir.join("memory"))),
    );
    let reflector =
        Reflector::from_core(core, conversations, dir.join("reflect-state.json")).unwrap();

    let outcome = reflector.run_now(2_000_000_000).await.unwrap();
    assert!(outcome.persona_changed);
    assert_eq!(outcome.notes, 2, "one durable note plus the persona note");

    let updated = std::fs::read_to_string(&persona_path).unwrap();
    assert!(updated.contains("quiet mornings"));
    let notes = reflector.memory.list();
    let persona_note = notes
        .iter()
        .find(|note| note.tags.iter().any(|tag| tag == PERSONA_TAG))
        .expect("the persona change was recorded in memory");
    assert!(persona_note.content.contains("it helps"), "why is recorded");
    assert!(
        persona_note.content.contains("added a line about mornings"),
        "how is recorded"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn persona_consideration_only_is_recorded_without_changing_the_file() {
    let dir = temp_dir("persona-think");
    let conversations = ConversationStore::new(dir.join("conversations"));
    conversations
        .save(&conversation(
            "t1",
            "2099-01-01T10:00:00Z",
            vec![("user", "hi"), ("assistant", "hello")],
        ))
        .unwrap();
    let persona_path = dir.join("persona.md");
    std::fs::write(&persona_path, "I am a pond frog.\n").unwrap();

    let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
    let request = ChatRequest::new(
        config.provider.model.clone(),
        vec![
            ChatMessage::system(REFLECTOR_SYSTEM),
            ChatMessage::user(digest_content_for_store_with_persona(
                &conversations,
                Some("I am a pond frog."),
            )),
        ],
    )
    .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
    // A block with only WHY/HOW: a reflection, no revision.
    let cassette = Cassette {
        cassette_version: crate::llm::CASSETTE_VERSION,
        recorded_at_unix: None,
        base_url: None,
        models: vec![],
        interactions: vec![Interaction {
            request,
            events: vec![
                CoreEvent::Delta {
                    text: "frogs\n---\nA note\n===PERSONA===\nWHY: I noticed I can be terse\nHOW: maybe soften my tone someday\n===END===\n"
                        .into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        }],
    };
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
    let core = Arc::new(
        AgentCore::with_mode(
            config,
            client,
            crate::llm::ClientMode::Fake {
                cassette: PathBuf::new(),
            },
        )
        .with_persona(persona_path.clone())
        .with_memory(MemoryStore::new(dir.join("memory"))),
    );
    let reflector =
        Reflector::from_core(core, conversations, dir.join("reflect-state.json")).unwrap();

    let outcome = reflector.run_now(2_000_000_000).await.unwrap();
    assert!(!outcome.persona_changed);
    assert_eq!(outcome.notes, 2);
    let updated = std::fs::read_to_string(&persona_path).unwrap();
    assert_eq!(updated, "I am a pond frog.\n", "file untouched");

    let notes = reflector.memory.list();
    let persona_note = notes
        .iter()
        .find(|note| note.tags.iter().any(|tag| tag == PERSONA_TAG))
        .expect("the persona reflection was recorded");
    assert!(persona_note.content.contains("I noticed I can be terse"));
    assert!(persona_note.content.contains("soften my tone someday"));
    assert!(persona_note.content.contains("left my persona unchanged"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn persona_edits_disabled_records_the_reflection_but_not_the_change() {
    let dir = temp_dir("persona-off");
    let conversations = ConversationStore::new(dir.join("conversations"));
    conversations
        .save(&conversation(
            "t1",
            "2099-01-01T10:00:00Z",
            vec![("user", "hi"), ("assistant", "hello")],
        ))
        .unwrap();
    let persona_path = dir.join("persona.md");
    std::fs::write(&persona_path, "I am a pond frog.\n").unwrap();

    let config = Config::parse("[reflect]\nenabled = true\npersona_edits = false\n").unwrap();
    let request = ChatRequest::new(
        config.provider.model.clone(),
        vec![
            ChatMessage::system(REFLECTOR_SYSTEM),
            ChatMessage::user(digest_content_for_store_with_persona(
                &conversations,
                Some("I am a pond frog."),
            )),
        ],
    )
    .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
    let cassette = Cassette {
        cassette_version: crate::llm::CASSETTE_VERSION,
        recorded_at_unix: None,
        base_url: None,
        models: vec![],
        interactions: vec![Interaction {
            request,
            events: vec![
                CoreEvent::Delta {
                    text: "frogs\n---\nA note\n===PERSONA===\nWHY: it helps\nHOW: nope\n---\nA different frog.\n===END===\n"
                        .into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        }],
    };
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
    let core = Arc::new(
        AgentCore::with_mode(
            config,
            client,
            crate::llm::ClientMode::Fake {
                cassette: PathBuf::new(),
            },
        )
        .with_persona(persona_path.clone())
        .with_memory(MemoryStore::new(dir.join("memory"))),
    );
    let reflector =
        Reflector::from_core(core, conversations, dir.join("reflect-state.json")).unwrap();

    let outcome = reflector.run_now(2_000_000_000).await.unwrap();
    assert!(!outcome.persona_changed);
    assert_eq!(outcome.notes, 2, "the reflection note is still recorded");
    let updated = std::fs::read_to_string(&persona_path).unwrap();
    assert_eq!(updated, "I am a pond frog.\n", "persona untouched");
    let notes = reflector.memory.list();
    let persona_note = notes
        .iter()
        .find(|note| note.tags.iter().any(|tag| tag == PERSONA_TAG))
        .expect("the persona reflection was still recorded");
    assert!(persona_note.content.contains("left my persona unchanged"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_failed_run_leaves_state_untouched() {
    let dir = temp_dir("failed");
    let conversations = ConversationStore::new(dir.join("conversations"));
    conversations
        .save(&conversation(
            "t1",
            "2099-01-01T10:00:00Z",
            vec![("user", "hi"), ("assistant", "hello")],
        ))
        .unwrap();
    let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
    // The provider fails: the run must not advance the state file.
    let request = ChatRequest::new(
        config.provider.model.clone(),
        vec![
            ChatMessage::system(REFLECTOR_SYSTEM),
            ChatMessage::user(digest_content_for_store(&conversations)),
        ],
    )
    .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
    let cassette = Cassette {
        cassette_version: crate::llm::CASSETTE_VERSION,
        recorded_at_unix: None,
        base_url: None,
        models: vec![],
        interactions: vec![Interaction {
            request,
            events: vec![CoreEvent::Error {
                kind: crate::error::ApiErrorKind::Provider,
                message: "boom".into(),
            }],
        }],
    };
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
    let reflector = reflector(config, client, &dir, conversations);

    let err = reflector.run_now(2_000_000_000).await.unwrap_err();
    assert_ne!(err.kind, crate::error::ApiErrorKind::Internal);
    assert_eq!(reflector.last_run(), 0, "state must not advance on failure");
    assert!(reflector.memory.list().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn disabled_run_reports_disabled_without_touching_state() {
    let dir = temp_dir("off");
    let config = Config::parse("[reflect]\nenabled = false\n").unwrap();
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
    let reflector = reflector(
        config,
        client,
        &dir,
        ConversationStore::new(dir.join("conversations")),
    );
    let outcome = reflector.run_now(2_000_000_000).await.unwrap();
    assert_eq!(outcome.status, ReflectStatus::Disabled);
    assert_eq!(reflector.last_run(), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn empty_run_advances_state_without_calling_the_worker() {
    let dir = temp_dir("empty");
    let conversations = ConversationStore::new(dir.join("conversations"));
    // A conversation with no exchange must not be a candidate.
    conversations
        .save(&conversation(
            "t1",
            "2099-01-01T10:00:00Z",
            vec![("user", "hi")],
        ))
        .unwrap();
    let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
    // No cassette: any worker call would be a loud error, proving none ran.
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::builtin());
    let reflector = reflector(config, client, &dir, conversations);
    let outcome = reflector.run_now(2_000_000_000).await.unwrap();
    assert_eq!(outcome.status, ReflectStatus::Ran);
    assert_eq!(outcome.conversations, 0);
    assert_eq!(outcome.notes, 0);
    assert_eq!(reflector.last_run(), 2_000_000_000);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn every_changed_conversation_is_digested_past_the_batch_cap() {
    let dir = temp_dir("batches");
    let conversations = ConversationStore::new(dir.join("conversations"));
    // One more changed conversation than a single worker batch holds, with
    // distinct timestamps so newest-first order (and the split) is stable.
    for i in 0..(REFLECT_MAX_CONVERSATIONS + 1) {
        save_with_updated(
            &conversations,
            &conversation(
                &format!("t{i:02}"),
                &format!("2099-01-{:02}T10:00:00Z", i + 1),
                vec![("user", "hi"), ("assistant", "hello")],
            ),
        );
    }
    let all = conversations.list().unwrap();
    assert_eq!(all.len(), REFLECT_MAX_CONVERSATIONS + 1);
    let batches = [
        all[..REFLECT_MAX_CONVERSATIONS].to_vec(),
        all[REFLECT_MAX_CONVERSATIONS..].to_vec(),
    ];

    let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
    let interactions = batches
        .iter()
        .enumerate()
        .map(|(i, batch)| Interaction {
            request: ChatRequest::new(
                config.provider.model.clone(),
                vec![
                    ChatMessage::system(REFLECTOR_SYSTEM),
                    ChatMessage::user(digest_content(batch, None)),
                ],
            )
            .with_max_tokens(Some(config.workers.reflector.max_output_tokens)),
            events: vec![
                CoreEvent::Delta {
                    text: format!("batch{i}\n---\nnote from batch {i}\n"),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        })
        .collect();
    let cassette = Cassette {
        cassette_version: crate::llm::CASSETTE_VERSION,
        recorded_at_unix: None,
        base_url: None,
        models: vec![],
        interactions,
    };
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
    let reflector = reflector(config, client, &dir, conversations);

    let outcome = reflector.run_now(2_000_000_000).await.unwrap();
    assert_eq!(outcome.status, ReflectStatus::Ran);
    assert_eq!(
        outcome.conversations,
        REFLECT_MAX_CONVERSATIONS + 1,
        "every changed conversation is digested, not just the newest batch"
    );
    assert_eq!(outcome.notes, 2);
    assert_eq!(reflector.memory.list().len(), 2);
    assert_eq!(reflector.last_run(), 2_000_000_000);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_conversation_from_the_last_runs_second_is_retried_not_skipped() {
    let dir = temp_dir("same-second");
    let conversations = ConversationStore::new(dir.join("conversations"));
    let now = 2_000_000_000u64;
    save_with_updated(
        &conversations,
        &conversation(
            "t1",
            &rfc3339_from_unix(now),
            vec![("user", "hi"), ("assistant", "hello")],
        ),
    );
    let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
    let request = ChatRequest::new(
        config.provider.model.clone(),
        vec![
            ChatMessage::system(REFLECTOR_SYSTEM),
            ChatMessage::user(digest_content_for_store(&conversations)),
        ],
    )
    .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
    let cassette = Cassette {
        cassette_version: crate::llm::CASSETTE_VERSION,
        recorded_at_unix: None,
        base_url: None,
        models: vec![],
        interactions: vec![Interaction {
            request,
            events: vec![
                CoreEvent::Delta {
                    text: "second\n---\nnoted in the same second\n".into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        }],
    };
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
    let reflector = reflector(config, client, &dir, conversations);
    // The previous run finished exactly in this conversation's second.
    reflector.write_state(now).unwrap();

    let outcome = reflector.run_now(now).await.unwrap();
    assert_eq!(
        outcome.conversations, 1,
        "a same-second conversation is retried, not skipped forever"
    );
    assert_eq!(outcome.notes, 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_retry_after_a_partial_run_does_not_duplicate_notes() {
    let dir = temp_dir("retry-dedupe");
    let conversations = ConversationStore::new(dir.join("conversations"));
    conversations
        .save(&conversation(
            "t1",
            "2099-01-01T10:00:00Z",
            vec![("user", "hi"), ("assistant", "hello")],
        ))
        .unwrap();
    let config = Config::parse("[reflect]\nenabled = true\n").unwrap();
    let request = ChatRequest::new(
        config.provider.model.clone(),
        vec![
            ChatMessage::system(REFLECTOR_SYSTEM),
            ChatMessage::user(digest_content_for_store(&conversations)),
        ],
    )
    .with_max_tokens(Some(config.workers.reflector.max_output_tokens));
    let cassette = Cassette {
        cassette_version: crate::llm::CASSETTE_VERSION,
        recorded_at_unix: None,
        base_url: None,
        models: vec![],
        interactions: vec![Interaction {
            request,
            events: vec![
                CoreEvent::Delta {
                    text: "one\n---\nfirst note\n===\ntwo\n---\nsecond note\n".into(),
                },
                CoreEvent::TurnDone { usage: None },
            ],
        }],
    };
    let client: Arc<dyn LlmClient> = Arc::new(FakeProvider::from_cassette(cassette));
    let reflector = reflector(config, client, &dir, conversations);

    let outcome = reflector.run_now(2_000_000_000).await.unwrap();
    assert_eq!(outcome.notes, 2);
    // Simulate a run that wrote its notes but crashed before advancing the
    // state: the retry must not store the same notes again.
    reflector.write_state(0).unwrap();
    let retry = reflector.run_now(2_000_000_000).await.unwrap();
    assert_eq!(retry.conversations, 1);
    assert_eq!(retry.notes, 0, "identical notes are not written twice");
    assert_eq!(reflector.memory.list().len(), 2);
    std::fs::remove_dir_all(&dir).ok();
}
