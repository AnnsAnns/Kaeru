use super::*;

fn temp_store(name: &str) -> (ConversationStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!("kaeru-test-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    (ConversationStore::new(&dir), dir)
}

fn sample(id: &str) -> Conversation {
    Conversation {
        schema: CONVERSATION_SCHEMA_VERSION,
        id: id.into(),
        title: Some("hello".into()),
        created_at: rfc3339_from_unix(1_000_000_000),
        updated_at: rfc3339_from_unix(1_000_000_000),
        summary: None,
        messages: vec![
            StoredMessage::from(&ChatMessage::user("hello")),
            StoredMessage::from(&ChatMessage::assistant("Hello!")),
        ],
        usage: Usage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            total_tokens: Some(15),
        },
    }
}

#[test]
fn save_and_load_round_trip() {
    let (store, dir) = temp_store("round-trip");
    store.save(&sample("default")).unwrap();
    let loaded = store.load("default").unwrap().unwrap();
    assert_eq!(loaded.messages, sample("default").messages);
    assert_eq!(loaded.messages[0].to_chat(), ChatMessage::user("hello"));
    // `save` stamps a fresh `updatedAt` (the store owns the clock).
    assert_ne!(loaded.updated_at, sample("default").updated_at);
    assert!(loaded.updated_at > loaded.created_at);
    assert!(store.load("missing").unwrap().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn saved_file_matches_the_planned_wire_schema() {
    let (store, dir) = temp_store("wire-schema");
    store.save(&sample("default")).unwrap();
    let text = std::fs::read_to_string(dir.join("default.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["schema"], 3);
    assert_eq!(json["id"], "default");
    assert_eq!(json["createdAt"], "2001-09-09T01:46:40Z");
    assert!(
        json["updatedAt"].as_str().unwrap() > "2001-09-09T01:46:40Z",
        "save must stamp a fresh updatedAt"
    );
    assert_eq!(json["summary"], serde_json::Value::Null);
    assert_eq!(json["messages"][0]["role"], "user");
    assert_eq!(json["messages"][0]["content"], "hello");
    assert_eq!(json["usage"]["input_tokens"], 10);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_v1_file_migrates_to_v3_with_no_data_loss() {
    let (store, dir) = temp_store("migrate-v1");
    // A v1 file: schema 1, no `updatedAt` field at all.
    let v1 = serde_json::json!({
        "schema": 1,
        "id": "default",
        "title": "old thread",
        "createdAt": "2001-09-09T01:46:40Z",
        "summary": null,
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "Hello!"}
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    std::fs::write(
        dir.join("default.json"),
        serde_json::to_string_pretty(&v1).unwrap(),
    )
    .unwrap();

    let loaded = store.load("default").unwrap().unwrap();
    assert_eq!(loaded.schema, 3);
    assert_eq!(loaded.updated_at, "2001-09-09T01:46:40Z");
    assert_eq!(loaded.created_at, "2001-09-09T01:46:40Z");
    assert_eq!(loaded.title.as_deref(), Some("old thread"));
    assert_eq!(loaded.messages.len(), 2);
    assert!(loaded.messages[0].artifacts.is_empty());
    assert_eq!(loaded.usage.input_tokens, Some(10));
    // Not quarantined: the file stays in place.
    assert!(dir.join("default.json").is_file());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn artifacts_round_trip_with_camel_case_mime_hints() {
    let (store, dir) = temp_store("artifacts");
    let mut conversation = sample("default");
    conversation.messages[0] = StoredMessage::from(
        &ChatMessage::user("look at this")
            .with_artifacts(vec![Artifact::new("photo.png", Some("image/png"))]),
    );
    conversation.messages[1] = StoredMessage::from(
        &ChatMessage::assistant("done")
            .with_artifacts(vec![Artifact::new("rotated.png", None)]),
    );
    store.save(&conversation).unwrap();

    let text = std::fs::read_to_string(dir.join("default.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["messages"][0]["artifacts"][0]["path"], "photo.png");
    assert_eq!(json["messages"][0]["artifacts"][0]["mimeHint"], "image/png");
    assert!(
        json["messages"][1]["artifacts"][0]
            .get("mimeHint")
            .is_none()
    );

    let loaded = store.load("default").unwrap().unwrap();
    assert_eq!(
        loaded.messages[0].to_chat().artifacts,
        vec![Artifact::new("photo.png", Some("image/png"))]
    );
    assert_eq!(
        loaded.messages[1].to_chat().artifacts,
        vec![Artifact::new("rotated.png", None)]
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_v2_file_loads_as_v3_with_empty_artifacts() {
    let (store, dir) = temp_store("migrate-v2");
    let mut v2 = serde_json::to_value(sample("default")).unwrap();
    v2["schema"] = serde_json::json!(2);
    // v2 files have no artifacts field anywhere.
    v2["messages"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .for_each(|m| {
            m.as_object_mut().unwrap().remove("artifacts");
        });
    std::fs::write(
        dir.join("default.json"),
        serde_json::to_string_pretty(&v2).unwrap(),
    )
    .unwrap();

    let loaded = store.load("default").unwrap().unwrap();
    assert_eq!(loaded.schema, 3);
    assert_eq!(loaded.messages.len(), 2);
    assert!(loaded.messages.iter().all(|m| m.artifacts.is_empty()));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_returns_threads_newest_first() {
    let (store, dir) = temp_store("list");
    // Write directly so the distinct timestamps survive (save stamps now).
    let mut older = sample("older");
    older.updated_at = "2020-01-01T00:00:00Z".into();
    let mut newer = sample("newer");
    newer.updated_at = "2024-01-01T00:00:00Z".into();
    for conversation in [older, newer] {
        std::fs::write(
            dir.join(format!("{}.json", conversation.id)),
            serde_json::to_string_pretty(&conversation).unwrap(),
        )
        .unwrap();
    }
    let listed: Vec<String> = store.list().unwrap().into_iter().map(|c| c.id).collect();
    assert_eq!(listed, vec!["newer".to_owned(), "older".to_owned()]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_skips_foreign_files_and_is_empty_when_absent() {
    let (store, dir) = temp_store("list-foreign");
    assert!(store.list().unwrap().is_empty());
    std::fs::write(dir.join("notes.txt"), "not a conversation").unwrap();
    std::fs::write(dir.join("bad id.json"), "{}").unwrap();
    assert!(store.list().unwrap().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn delete_removes_the_file_and_is_idempotent() {
    let (store, dir) = temp_store("delete");
    store.save(&sample("default")).unwrap();
    store.delete("default").unwrap();
    assert!(!dir.join("default.json").exists());
    store.delete("default").unwrap(); // missing is a no-op
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn save_is_atomic_and_leaves_no_tmp_files() {
    let (store, dir) = temp_store("atomic");
    store.save(&sample("default")).unwrap();
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["default.json".to_owned()]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn save_overwrites_the_previous_version_in_place() {
    let (store, dir) = temp_store("overwrite");
    store.save(&sample("default")).unwrap();
    let mut updated = sample("default");
    updated
        .messages
        .push(StoredMessage::from(&ChatMessage::user("again")));
    store.save(&updated).unwrap();
    assert_eq!(store.load("default").unwrap().unwrap().messages.len(), 3);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unreadable_files_are_quarantined_not_fatal() {
    let (store, dir) = temp_store("quarantine");
    std::fs::write(dir.join("default.json"), "{not json at all").unwrap();
    assert!(store.load("default").unwrap().is_none());
    assert!(
        dir.join("default.json.quarantine").is_file(),
        "broken file must be moved aside, not kept in rotation"
    );
    assert!(!dir.join("default.json").exists());
    // A retry now reports "absent" instead of failing again.
    assert!(store.load("default").unwrap().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unknown_schema_versions_are_quarantined() {
    let (store, dir) = temp_store("unknown-schema");
    let mut future = sample("default");
    future.schema = 99;
    std::fs::write(
        dir.join("default.json"),
        serde_json::to_string(&future).unwrap(),
    )
    .unwrap();
    assert!(store.load("default").unwrap().is_none());
    assert!(dir.join("default.json.quarantine").is_file());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_schema_version_wider_than_u32_does_not_wrap_into_a_valid_one() {
    let (store, dir) = temp_store("schema-wrap");
    // 2^32 + 2 truncated to u32 would look exactly like a valid v2 file.
    let mut value = serde_json::to_value(sample("default")).unwrap();
    value["schema"] = serde_json::json!(4_294_967_298u64);
    std::fs::write(
        dir.join("default.json"),
        serde_json::to_string(&value).unwrap(),
    )
    .unwrap();
    assert!(
        store.load("default").unwrap().is_none(),
        "a wrapped schema must be quarantined, not loaded"
    );
    assert!(dir.join("default.json.quarantine").is_file());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn path_trick_ids_are_rejected() {
    let (store, dir) = temp_store("bad-ids");
    for id in ["", "../escape", "a/b", ".hidden", "with space", "dot.id"] {
        assert!(
            store.save(&sample(id)).is_err(),
            "id {id:?} must be rejected"
        );
        assert!(store.load(id).is_err(), "id {id:?} must be rejected");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn rfc3339_helper_matches_known_timestamps() {
    assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339_from_unix(1_000_000_000), "2001-09-09T01:46:40Z");
    assert_eq!(rfc3339_from_unix(1_759_011_840), "2025-09-27T22:24:00Z");
    assert_eq!(rfc3339_from_unix(951_782_399), "2000-02-28T23:59:59Z");
    // Leap day: 2000-02-29 existed (divisible by 400).
    assert_eq!(rfc3339_from_unix(951_868_800), "2000-03-01T00:00:00Z");
}
