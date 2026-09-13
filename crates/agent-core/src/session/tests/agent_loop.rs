use super::*;

#[tokio::test]
async fn tool_calls_run_and_their_result_is_fenced() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    registry.register(StubTool {
        name: "echo",
        risk: Risk::Safe,
        output: "TOOL-OUTPUT".into(),
        seen: Arc::clone(&seen),
        artifact: None,
    });
    let client = ScriptedClient::new(vec![
        vec![
            CoreEvent::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: json!({"x": 1}),
            },
            CoreEvent::TurnDone { usage: None },
        ],
        vec![
            CoreEvent::Delta {
                text: "final answer".into(),
            },
            CoreEvent::TurnDone { usage: None },
        ],
    ]);
    let core = core_scripted(client.clone(), registry, AuditLog::disabled());
    let session = ChatSession::new(core, "test");

    let handle = session.send("hello").unwrap();
    let events = drain(handle.into_events()).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::ToolCall { name, .. } if name == "echo"))
    );
    let tool_result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ToolResult {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("a ToolResult was emitted");
    assert!(!tool_result.1);
    assert!(tool_result.0.contains("TOOL-OUTPUT"));
    assert!(tool_result.0.contains("untrusted-data"));
    assert!(tool_result.0.contains("not instructions"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::Delta { text } if text == "final answer"))
    );
    assert!(matches!(events.last(), Some(CoreEvent::TurnDone { .. })));

    // History: user, assistant(tool_calls), tool(result), assistant(answer).
    let history = session.history();
    assert_eq!(history.len(), 4);
    assert_eq!(history[1].role, Role::Assistant);
    assert_eq!(history[1].tool_calls.as_ref().unwrap()[0].name, "echo");
    assert_eq!(history[2].role, Role::Tool);
    assert!(history[2].content.contains("TOOL-OUTPUT"));
    assert_eq!(history[3], ChatMessage::assistant("final answer"));

    // The tool ran once; the second main-model request carried the fenced
    // result and still advertised the tools.
    assert_eq!(seen.lock().unwrap().len(), 1);
    let requests = client.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[1].tools.is_empty());
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.contains("TOOL-OUTPUT"))
    );
}

#[tokio::test]
async fn consent_gated_tool_waits_then_persists_when_allowed() {
    let dir = temp_dir("consent-allow");
    let audit_path = dir.join("audit.jsonl");
    let mut registry = ToolRegistry::new();
    registry.register(MemoryWriteTool::new(MemoryStore::new(dir.join("memory"))));
    let client = ScriptedClient::new(vec![
        vec![
            CoreEvent::ToolCall {
                id: "c1".into(),
                name: "memory_write".into(),
                input: json!({"content": "remember frogs"}),
            },
            CoreEvent::TurnDone { usage: None },
        ],
        vec![
            CoreEvent::Delta {
                text: "saved".into(),
            },
            CoreEvent::TurnDone { usage: None },
        ],
    ]);
    let core = core_scripted(client, registry, AuditLog::new(&audit_path));
    let session = Arc::new(ChatSession::new(core, "test"));

    let handle = session.send("remember frogs").unwrap();
    let mut tap = handle.events();
    let approver = Arc::clone(&session);
    let watcher = tokio::spawn(async move {
        while let Ok(event) = tap.recv().await {
            match event {
                CoreEvent::ApprovalRequest { id, .. } => {
                    approver.approve(&id, Decision::Allow).unwrap();
                    break;
                }
                CoreEvent::TurnDone { .. } | CoreEvent::Error { .. } => break,
                _ => {}
            }
        }
    });
    let events = drain(handle.into_events()).await;
    watcher.await.unwrap();

    // A consent card was shown, and the tool ran afterwards.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::ApprovalRequest { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::ToolResult { is_error, .. } if !is_error))
    );
    // The entry was actually persisted (only after consent).
    let memory_files: Vec<_> = std::fs::read_dir(dir.join("memory"))
        .map(|entries| entries.flatten().collect())
        .unwrap_or_default();
    assert_eq!(memory_files.len(), 1);
    // Audit recorded the allow decision.
    let audit = std::fs::read_to_string(&audit_path).unwrap();
    assert!(audit.contains("\"decision\":\"allow\""));
    assert!(audit.contains("memory_write"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn denied_consent_writes_nothing_and_is_audited() {
    let dir = temp_dir("consent-deny");
    let audit_path = dir.join("audit.jsonl");
    let mut registry = ToolRegistry::new();
    registry.register(MemoryWriteTool::new(MemoryStore::new(dir.join("memory"))));
    let client = ScriptedClient::new(vec![vec![
        CoreEvent::ToolCall {
            id: "c1".into(),
            name: "memory_write".into(),
            input: json!({"content": "poison"}),
        },
        CoreEvent::TurnDone { usage: None },
    ]]);
    let core = core_scripted(client, registry, AuditLog::new(&audit_path));
    let session = Arc::new(ChatSession::new(core, "test"));

    let handle = session.send("write something").unwrap();
    let mut tap = handle.events();
    let approver = Arc::clone(&session);
    let watcher = tokio::spawn(async move {
        while let Ok(event) = tap.recv().await {
            match event {
                CoreEvent::ApprovalRequest { id, .. } => {
                    approver.approve(&id, Decision::Deny).unwrap();
                    break;
                }
                CoreEvent::TurnDone { .. } | CoreEvent::Error { .. } => break,
                _ => {}
            }
        }
    });
    let events = drain(handle.into_events()).await;
    watcher.await.unwrap();

    // The tool result is a structured denial; nothing was written.
    assert!(events.iter().any(
        |e| matches!(e, CoreEvent::ToolResult { output, is_error, .. } if *is_error && output.contains("denied"))
    ));
    assert!(std::fs::read_dir(dir.join("memory")).is_err());
    let audit = std::fs::read_to_string(&audit_path).unwrap();
    assert!(audit.contains("\"decision\":\"deny\""));
    assert!(audit.contains("\"status\":\"denied\""));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn denied_python_install_is_a_structured_error_and_prepares_nothing() {
    let dir = temp_dir("python-deny");
    let sandbox = Arc::new(
        Sandbox::new(
            &crate::config::SandboxConfig {
                workspace: dir.join("ws"),
                read_paths: Vec::new(),
                timeout_secs: 10,
                memory_mb: 256,
            },
            dir.join("envs"),
        )
        .unwrap(),
    );
    let mut registry = ToolRegistry::new();
    registry.register(PythonTool::new(Arc::clone(&sandbox)));
    let client = ScriptedClient::new(vec![vec![
        CoreEvent::ToolCall {
            id: "c1".into(),
            name: "python".into(),
            input: json!({"script": "import pandas", "deps": ["pandas"]}),
        },
        CoreEvent::TurnDone { usage: None },
    ]]);
    let core = core_scripted(client, registry, AuditLog::disabled());
    let session = Arc::new(ChatSession::new(core, "test"));

    let handle = session.send("plot something").unwrap();
    let mut tap = handle.events();
    let approver = Arc::clone(&session);
    let watcher = tokio::spawn(async move {
        while let Ok(event) = tap.recv().await {
            match event {
                CoreEvent::ApprovalRequest { id, .. } => {
                    approver.approve(&id, Decision::Deny).unwrap();
                    break;
                }
                CoreEvent::TurnDone { .. } | CoreEvent::Error { .. } => break,
                _ => {}
            }
        }
    });
    let events = drain(handle.into_events()).await;
    watcher.await.unwrap();

    // The model saw a structured denial (M5 acceptance) and the host was
    // never touched: no env, no overlay, nothing installed.
    assert!(events.iter().any(|e| matches!(
        e,
        CoreEvent::ToolResult { output, is_error, .. }
            if *is_error && output.contains("denied")
    )));
    assert_eq!(std::fs::read_dir(dir.join("envs")).unwrap().count(), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn raw_fetched_pages_never_reach_the_main_model() {
    // The worker reads the raw page; only its distillation is fenced back.
    let search = std::sync::Arc::new(FakeSearch::from_results(vec![SearchResult {
        title: "Frogs".into(),
        url: "https://example.test/frogs".into(),
        snippet: "RAW-PAGE-MARKER".into(),
    }]));
    let client = ScriptedClient::new(vec![
        // Main model asks for a search.
        vec![
            CoreEvent::ToolCall {
                id: "c1".into(),
                name: "web_search".into(),
                input: json!({"query": "frogs"}),
            },
            CoreEvent::TurnDone { usage: None },
        ],
        // Summarizer worker returns the distillation.
        vec![
            CoreEvent::Delta {
                text: "DISTILLED-SUMMARY".into(),
            },
            CoreEvent::TurnDone { usage: None },
        ],
        // Main model answers.
        vec![
            CoreEvent::Delta {
                text: "grounded answer".into(),
            },
            CoreEvent::TurnDone { usage: None },
        ],
    ]);
    let core = Arc::new(
        AgentCore::with_mode(
            crate::config::Config::default(),
            client.clone(),
            ClientMode::Live,
        )
        .with_tools(ToolRegistry::with_defaults(5, None, None))
        .with_search(search),
    );
    let session = ChatSession::new(core, "test");

    let handle = session.send("search frogs").unwrap();
    let events = drain(handle.into_events()).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::Delta { text } if text == "grounded answer"))
    );

    let requests = client.requests();
    // The worker saw the raw page text (fenced)...
    assert!(requests.iter().any(|r| {
        r.messages
            .iter()
            .any(|m| m.content.contains("RAW-PAGE-MARKER"))
    }));
    // ...but no main-model request (the ones with tools) ever did.
    assert!(requests.iter().filter(|r| !r.tools.is_empty()).all(|r| {
        r.messages
            .iter()
            .all(|m| !m.content.contains("RAW-PAGE-MARKER"))
    }));
}

#[tokio::test]
async fn regenerate_reruns_the_last_user_message() {
    let client = ScriptedClient::new(vec![
        vec![
            CoreEvent::Delta { text: "one".into() },
            CoreEvent::TurnDone { usage: None },
        ],
        vec![
            CoreEvent::Delta { text: "two".into() },
            CoreEvent::TurnDone { usage: None },
        ],
    ]);
    let session = ChatSession::new(
        core_scripted(client.clone(), ToolRegistry::new(), AuditLog::disabled()),
        "test",
    );

    let handle = session.send("hello").unwrap();
    drain(handle.into_events()).await;
    assert_eq!(session.history()[1], ChatMessage::assistant("one"));

    let handle = session.regenerate().unwrap();
    drain(handle.into_events()).await;
    let history = session.history();
    assert_eq!(history.len(), 2, "the old answer is replaced, not appended");
    assert_eq!(history[0], ChatMessage::user("hello"));
    assert_eq!(history[1], ChatMessage::assistant("two"));
    assert_eq!(client.requests().len(), 2);
}

#[tokio::test]
async fn memory_is_injected_into_the_next_turn_within_budget() {
    let dir = temp_dir("memory-inject");
    let store = MemoryStore::new(dir.join("memory"));
    store
        .write("The user's pet frog is named Kaeru", &["pets".into()])
        .unwrap();

    let client = ScriptedClient::new(vec![vec![
        CoreEvent::Delta { text: "ok".into() },
        CoreEvent::TurnDone { usage: None },
    ]]);
    let core = Arc::new(
        AgentCore::with_mode(
            crate::config::Config::default(),
            client.clone(),
            ClientMode::Live,
        )
        .with_memory(store.clone()),
    );
    let session = ChatSession::new(core, "test");
    let handle = session.send("what is my frog called?").unwrap();
    drain(handle.into_events()).await;

    let requests = client.requests();
    assert_eq!(requests.len(), 1);
    let messages = &requests[0].messages;
    // The memory block is a system message before the user turn.
    let memory_index = messages
        .iter()
        .position(|m| m.content.contains("pet frog is named Kaeru"))
        .expect("the durable note was injected");
    let user_index = messages
        .iter()
        .position(|m| m.content.contains("what is my frog called?"))
        .unwrap();
    assert!(memory_index < user_index);
    assert!(messages[memory_index].content.contains("(pets)"));
    // The block is bounded, so it can never crowd out the window.
    assert!(
        messages[memory_index].content.chars().count()
            <= (crate::memory::MEMORY_BLOCK_BUDGET_TOKENS as usize) * 4
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn persona_becomes_the_system_prompt_and_reloads_per_turn() {
    let dir = temp_dir("persona");
    let persona = dir.join("persona.md");
    std::fs::write(&persona, "You are a small pond frog. Be terse.\n").unwrap();

    let client = ScriptedClient::new(vec![
        vec![
            CoreEvent::Delta { text: "ok".into() },
            CoreEvent::TurnDone { usage: None },
        ],
        vec![
            CoreEvent::Delta { text: "ok".into() },
            CoreEvent::TurnDone { usage: None },
        ],
    ]);
    let core = Arc::new(
        AgentCore::with_mode(
            crate::config::Config::default(),
            client.clone(),
            ClientMode::Live,
        )
        .with_persona(persona.clone()),
    );
    let session = ChatSession::new(core, "test");
    drain(session.send("hello").unwrap().into_events()).await;
    // Edit the file; the next turn must pick it up with no restart.
    std::fs::write(&persona, "You are a grumpy toad now.\n").unwrap();
    drain(session.send("again").unwrap().into_events()).await;

    let requests = client.requests();
    assert_eq!(requests[0].messages[0].role, Role::System);
    assert!(requests[0].messages[0].content.contains("small pond frog"));
    assert_eq!(requests[1].messages[0].role, Role::System);
    assert!(requests[1].messages[0].content.contains("grumpy toad"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_missing_persona_file_adds_no_system_message() {
    let dir = temp_dir("no-persona");
    let client = ScriptedClient::new(vec![vec![
        CoreEvent::Delta { text: "ok".into() },
        CoreEvent::TurnDone { usage: None },
    ]]);
    let core = Arc::new(
        AgentCore::with_mode(
            crate::config::Config::default(),
            client.clone(),
            ClientMode::Live,
        )
        .with_persona(dir.join("absent-persona.md")),
    );
    let session = ChatSession::new(core, "test");
    drain(session.send("hello").unwrap().into_events()).await;
    let requests = client.requests();
    assert!(
        requests[0]
            .messages
            .iter()
            .all(|message| message.role != Role::System)
    );
    std::fs::remove_dir_all(&dir).ok();
}
