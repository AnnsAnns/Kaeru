//! Schema-versioned conversation persistence (M2, §5.4 / §8 "Persistence").
//!
//! One JSON file per conversation under `data/conversations/{id}.json`:
//! plain files, atomic writes (tmp + rename), a `schema` version with a
//! migration hook. Unreadable or unknown-schema files are **quarantined**
//! (renamed aside) with a warning — a broken file never crashes a turn; the
//! session just starts empty.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiErrorKind, Result};
use crate::events::{Artifact, Usage};
use crate::llm::{ChatMessage, Role, ToolCall};

/// The conversation file version this build reads and writes.
///
/// v2 (M2.5) adds `updatedAt`; v1 files are migrated on load by copying
/// `createdAt` into `updatedAt`. v3 (M5) adds `artifacts` on messages —
/// workspace files attached to a user upload or an assistant answer.
pub const CONVERSATION_SCHEMA_VERSION: u32 = 3;

/// A workspace file reference as persisted (M5): `mimeHint` follows the
/// camelCase file convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredArtifact {
    pub path: String,
    #[serde(rename = "mimeHint", default, skip_serializing_if = "Option::is_none")]
    pub mime_hint: Option<String>,
}

impl From<&Artifact> for StoredArtifact {
    fn from(artifact: &Artifact) -> Self {
        Self {
            path: artifact.path.clone(),
            mime_hint: artifact.mime_hint.clone(),
        }
    }
}

impl StoredArtifact {
    pub fn to_artifact(&self) -> Artifact {
        Artifact {
            path: self.path.clone(),
            mime_hint: self.mime_hint.clone(),
        }
    }
}

/// One stored message. Mirrors [`ChatMessage`]; the agent-loop fields
/// (`toolCallId`, `toolCalls`) arrive with M3 and are optional/backward
/// compatible — older files simply lack them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: Role,
    pub content: String,
    #[serde(
        rename = "toolCallId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_call_id: Option<String>,
    /// Tool calls the assistant requested (M3).
    #[serde(rename = "toolCalls", default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Model thinking for this turn (display-only); absent on older files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Workspace files attached to this message (M5): uploads on user
    /// messages, artifacts on the assistant answer; absent on older files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<StoredArtifact>,
}

impl From<&ChatMessage> for StoredMessage {
    fn from(message: &ChatMessage) -> Self {
        Self {
            role: message.role,
            content: message.content.clone(),
            tool_call_id: message.tool_call_id.clone(),
            tool_calls: message.tool_calls.clone(),
            reasoning: message.reasoning.clone(),
            artifacts: message.artifacts.iter().map(StoredArtifact::from).collect(),
        }
    }
}

impl StoredMessage {
    pub fn to_chat(&self) -> ChatMessage {
        let mut message = ChatMessage::new(self.role, self.content.clone());
        message.reasoning = self.reasoning.clone();
        message.tool_call_id = self.tool_call_id.clone();
        message.tool_calls = self.tool_calls.clone();
        message.artifacts = self
            .artifacts
            .iter()
            .map(StoredArtifact::to_artifact)
            .collect();
        message
    }
}

/// A whole conversation as stored on disk (`data/conversations/{id}.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub schema: u32,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Last write timestamp; stamped by [`ConversationStore::save`] on every
    /// change and used to order threads newest-first (M2.5).
    #[serde(rename = "updatedAt", default)]
    pub updated_at: String,
    /// Rolling summary of turns that fell out of the context window [M2].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
    /// Token usage accumulated over all turns (missing reports count as 0).
    #[serde(default)]
    pub usage: Usage,
}

/// File-backed store for conversations under one directory.
#[derive(Debug, Clone)]
pub struct ConversationStore {
    dir: PathBuf,
}

impl ConversationStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Save atomically: write `{id}.json.tmp`, then rename over the target.
    /// A crash mid-save never leaves a half-written conversation behind.
    ///
    /// The store owns the clock: `updatedAt` is stamped here, on every write,
    /// so the sidebar re-sorts naturally (M2.5, ADR-024).
    pub fn save(&self, conversation: &Conversation) -> Result<()> {
        validate_id(&conversation.id)?;
        let mut stamped = conversation.clone();
        stamped.updated_at = now_rfc3339();
        let text = serde_json::to_string_pretty(&stamped)
            .map_err(|e| ApiError::internal(format!("cannot serialize conversation: {e}")))?;
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            ApiError::new(
                ApiErrorKind::Internal,
                format!(
                    "cannot create conversations dir {}: {e}",
                    self.dir.display()
                ),
            )
        })?;
        let path = self.path_for(&conversation.id)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &text).map_err(|e| {
            ApiError::new(
                ApiErrorKind::Internal,
                format!("cannot write {}: {e}", tmp.display()),
            )
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            ApiError::new(
                ApiErrorKind::Internal,
                format!("cannot finalize {}: {e}", path.display()),
            )
        })
    }

    /// Load a conversation. `Ok(None)` when absent — or when the file is
    /// unreadable/unknown-schema, in which case it is quarantined with a
    /// warning instead of crashing the session.
    pub fn load(&self, id: &str) -> Result<Option<Conversation>> {
        validate_id(id)?;
        let path = self.path_for(id)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ApiError::new(
                    ApiErrorKind::Internal,
                    format!("cannot read {}: {e}", path.display()),
                ));
            }
        };
        match migrate(&text) {
            Ok(conversation) => Ok(Some(conversation)),
            Err(err) => {
                tracing::warn!(
                    target: "agent_core::conversations",
                    "conversation file {} is not loadable ({err}); quarantining it",
                    path.display()
                );
                self.quarantine(&path);
                Ok(None)
            }
        }
    }

    /// All conversations in the store, newest `updatedAt` first (M2.5): the
    /// sidebar listing. Files that fail to load are already quarantined by
    /// [`ConversationStore::load`] and simply do not appear.
    pub fn list(&self) -> Result<Vec<Conversation>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(ApiError::internal(format!(
                    "cannot read conversations dir {}: {e}",
                    self.dir.display()
                )));
            }
        };
        let mut conversations = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                ApiError::internal(format!(
                    "cannot read conversations dir {}: {e}",
                    self.dir.display()
                ))
            })?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Foreign file names are skipped, not fatal (`load` would error).
            if validate_id(id).is_err() {
                continue;
            }
            if let Some(conversation) = self.load(id)? {
                conversations.push(conversation);
            }
        }
        // RFC 3339 UTC sorts lexicographically = chronologically.
        conversations.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(conversations)
    }

    /// Remove a conversation file. Missing files are a no-op (idempotent).
    pub fn delete(&self, id: &str) -> Result<()> {
        let path = self.path_for(id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ApiError::internal(format!(
                "cannot delete {}: {e}",
                path.display()
            ))),
        }
    }

    fn path_for(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(self.dir.join(format!("{id}.json")))
    }

    fn quarantine(&self, path: &Path) {
        let quarantined = path.with_extension("json.quarantine");
        match std::fs::rename(path, &quarantined) {
            Ok(()) => tracing::warn!(
                target: "agent_core::conversations",
                "moved {} out of the way (fix or delete it by hand)",
                quarantined.display()
            ),
            Err(e) => tracing::warn!(
                target: "agent_core::conversations",
                "cannot quarantine {}: {e}; it will fail again on the next load",
                path.display()
            ),
        }
    }
}

/// Migration hook (§5.4): older `schema` values are upgraded in place, newer
/// or malformed ones are a load error (which the store turns into a
/// quarantine). Today it chains `1 → 2` (adds `updatedAt`); future versions
/// append steps here.
fn migrate(text: &str) -> Result<Conversation> {
    let mut value: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        ApiError::new(
            ApiErrorKind::Internal,
            format!("not a valid conversation file: {e}"),
        )
    })?;
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| ApiError::internal("conversation file has no schema version"))?;
    if schema == 0 {
        return Err(ApiError::internal(
            "schema version 0 is not a valid conversation",
        ));
    }
    // Compare before any narrowing cast: a crafted `schema = 2^32 + 2` must
    // not wrap to 2 and load as a valid v2 file.
    if schema > u64::from(CONVERSATION_SCHEMA_VERSION) {
        return Err(ApiError::new(
            ApiErrorKind::Internal,
            format!(
                "schema version {schema} is unknown (this build speaks {CONVERSATION_SCHEMA_VERSION})"
            ),
        ));
    }
    if schema < 2 {
        migrate_v1_to_v2(&mut value);
    }
    if schema < 3 {
        migrate_v2_to_v3(&mut value);
    }
    serde_json::from_value(value).map_err(|e| {
        ApiError::new(
            ApiErrorKind::Internal,
            format!("not a valid conversation file: {e}"),
        )
    })
}

/// v1 → v2: add `updatedAt`, seeded from `createdAt` when absent, and bump the
/// schema marker. No data is lost: message bodies and usage are untouched.
fn migrate_v1_to_v2(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if object
        .get("updatedAt")
        .is_none_or(serde_json::Value::is_null)
    {
        let created_at = object
            .get("createdAt")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        object.insert("updatedAt".into(), created_at);
    }
    object.insert("schema".into(), serde_json::json!(2));
}

/// v2 → v3: the schema marker only. v3 adds optional per-message `artifacts`;
/// absent means empty, so no message data is rewritten.
fn migrate_v2_to_v3(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.insert("schema".into(), serde_json::json!(3));
}

/// Conversation ids become file names: keep them to plain `[A-Za-z0-9_-]`
/// so no path tricks are possible, no matter which frontend invents them.
fn validate_id(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(ApiError::config(format!(
            "conversation id {id:?} is invalid (allowed: letters, digits, '-', '_', max 128 chars)"
        )))
    }
}

/// Current wall-clock time as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
pub fn now_rfc3339() -> String {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_unix(unix)
}

/// Unix seconds → RFC 3339 UTC. Hand-rolled date math (Howard Hinnant's
/// civil-from-days algorithm) to stay on the boring dependency list (C15).
pub fn rfc3339_from_unix(unix: u64) -> String {
    let days = unix / 86_400;
    let secs = unix % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
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
}
