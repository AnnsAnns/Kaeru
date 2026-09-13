use super::*;

/* ---------- ConversationRegistry (M2.5, §5.5 / ADR-024) ---------- */

fn registry(name: &str) -> (ConversationRegistry, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("kaeru-test-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let registry = ConversationRegistry::new(
        core_with(FakeProvider::builtin()),
        ConversationStore::new(&dir),
    );
    (registry, dir)
}

#[test]
fn registry_create_persists_an_empty_thread_and_lists_it() {
    let (registry, dir) = registry("registry-create");
    let session = registry.create(None).unwrap();
    let id = session.conversation_id();
    assert!(dir.join(format!("{id}.json")).is_file());

    let listed = registry.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].message_count, 0);
    assert!(listed[0].title.is_none());

    // A named thread keeps its explicit title.
    let named = registry.create(Some("My thread".into())).unwrap();
    assert_eq!(named.title().as_deref(), Some("My thread"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn registry_get_is_cached_and_unknown_ids_are_not_found() {
    let (registry, dir) = registry("registry-get");
    let created = registry.create(None).unwrap();
    let id = created.conversation_id();

    let fetched = registry.get(&id).unwrap();
    assert!(Arc::ptr_eq(&created, &fetched), "session must be cached");

    let err = registry.get("does-not-exist").err().unwrap();
    assert_eq!(err.kind, ApiErrorKind::NotFound);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn registry_delete_drops_the_session_and_the_file() {
    let (registry, dir) = registry("registry-delete");
    let session = registry.create(None).unwrap();
    let id = session.conversation_id();

    registry.delete(&id).unwrap();
    assert!(!dir.join(format!("{id}.json")).exists());
    assert!(registry.get(&id).is_err());
    assert!(registry.list().unwrap().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

fn stamp_updated_at(dir: &std::path::Path, id: &str, timestamp: &str) {
    let path = dir.join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path).unwrap();
    let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["updatedAt"] = serde_json::Value::String(timestamp.into());
    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
}

#[test]
fn registry_picks_the_newest_thread_and_falls_back_after_delete() {
    let (registry, dir) = registry("registry-latest");
    assert!(registry.latest().unwrap().is_none());
    let older = registry.create(None).unwrap();
    let newer = registry.create(None).unwrap();
    stamp_updated_at(&dir, &older.conversation_id(), "2020-01-01T00:00:00Z");
    stamp_updated_at(&dir, &newer.conversation_id(), "2024-01-01T00:00:00Z");

    let latest = registry.latest().unwrap().unwrap();
    assert_eq!(latest.conversation_id(), newer.conversation_id());

    registry.delete(&newer.conversation_id()).unwrap();
    let latest = registry.latest().unwrap().unwrap();
    assert_eq!(latest.conversation_id(), older.conversation_id());
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn threads_keep_independent_histories() {
    let dir = std::env::temp_dir().join(format!("kaeru-test-{}-threads", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let registry = ConversationRegistry::new(
        core_with(FakeProvider::builtin()),
        ConversationStore::new(&dir),
    );

    let a = registry.create(None).unwrap();
    let b = registry.create(None).unwrap();
    let handle = a.send("only in a").unwrap();
    drain(handle.into_events()).await;

    assert_eq!(a.history().len(), 2);
    assert!(b.history().is_empty());
    assert_eq!(registry.list().unwrap().len(), 2);
    std::fs::remove_dir_all(&dir).ok();
}
