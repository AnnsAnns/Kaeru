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
use crate::events::Usage;
use crate::llm::{ChatMessage, Role};

/// The conversation file version this build reads and writes.
pub const CONVERSATION_SCHEMA_VERSION: u32 = 1;

/// One stored message. Mirrors [`ChatMessage`]; `tool_call_id` arrives with
/// the agent loop (M3) but is part of the wire schema from day one.
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
}

impl From<&ChatMessage> for StoredMessage {
    fn from(message: &ChatMessage) -> Self {
        Self {
            role: message.role,
            content: message.content.clone(),
            tool_call_id: None,
        }
    }
}

impl StoredMessage {
    pub fn to_chat(&self) -> ChatMessage {
        ChatMessage::new(self.role, self.content.clone())
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
    pub fn save(&self, conversation: &Conversation) -> Result<()> {
        validate_id(&conversation.id)?;
        let text = serde_json::to_string_pretty(conversation)
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

/// Migration hook (§5.4): unknown `schema` values are a load error (which the
/// store turns into a quarantine). Future versions chain `schema: n → n+1`
/// steps here; today only version 1 exists.
fn migrate(text: &str) -> Result<Conversation> {
    let conversation: Conversation = serde_json::from_str(text).map_err(|e| {
        ApiError::new(
            ApiErrorKind::Internal,
            format!("not a valid conversation file: {e}"),
        )
    })?;
    if conversation.schema == CONVERSATION_SCHEMA_VERSION {
        Ok(conversation)
    } else {
        Err(ApiError::new(
            ApiErrorKind::Internal,
            format!(
                "schema version {} is unknown (this build speaks {CONVERSATION_SCHEMA_VERSION})",
                conversation.schema
            ),
        ))
    }
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
        assert_eq!(loaded, sample("default"));
        assert_eq!(loaded.messages[0].to_chat(), ChatMessage::user("hello"));
        assert!(store.load("missing").unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saved_file_matches_the_planned_wire_schema() {
        let (store, dir) = temp_store("wire-schema");
        store.save(&sample("default")).unwrap();
        let text = std::fs::read_to_string(dir.join("default.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["id"], "default");
        assert_eq!(json["createdAt"], "2001-09-09T01:46:40Z");
        assert_eq!(json["summary"], serde_json::Value::Null);
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "hello");
        assert_eq!(json["usage"]["input_tokens"], 10);
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
