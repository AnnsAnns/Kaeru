//! The thread registry: a process-wide cache of live sessions over the
//! plain-file store (M2.5, §5.5 / ADR-024).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::AgentCore;
use crate::conversations::ConversationStore;
use crate::error::{ApiError, ApiErrorKind, Result};
use crate::events::Usage;

use super::ChatSession;

/// One row of the thread sidebar (M2.5): header fields only, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "messageCount")]
    pub message_count: usize,
    #[serde(default)]
    pub usage: Usage,
}

/// Cache of live sessions over the plain-file store (M2.5, ADR-024). One
/// `Arc<ChatSession>` per conversation id, lazily loaded, so an in-flight
/// turn stays reachable across HTTP requests; the store stays the source of
/// truth. Entries live for the process lifetime (no eviction) — deliberate
/// at personal scale, see arc42 §11.
pub struct ConversationRegistry {
    core: Arc<AgentCore>,
    store: ConversationStore,
    sessions: Mutex<HashMap<String, Arc<ChatSession>>>,
}

impl ConversationRegistry {
    pub fn new(core: Arc<AgentCore>, store: ConversationStore) -> Self {
        Self {
            core,
            store,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn store(&self) -> &ConversationStore {
        &self.store
    }

    /// Sidebar rows, newest `updatedAt` first.
    pub fn list(&self) -> Result<Vec<ThreadSummary>> {
        let conversations = self.store.list()?;
        Ok(conversations
            .into_iter()
            .map(|c| ThreadSummary {
                id: c.id,
                title: c.title,
                created_at: c.created_at,
                updated_at: c.updated_at,
                message_count: c.messages.len(),
                usage: c.usage,
            })
            .collect())
    }

    /// Create a new empty thread and persist it immediately so it appears in
    /// listings before its first turn.
    pub fn create(&self, title: Option<String>) -> Result<Arc<ChatSession>> {
        let id = new_thread_id();
        let session = Arc::new(ChatSession::with_store(
            Arc::clone(&self.core),
            id.clone(),
            self.store.clone(),
        ));
        if title.is_some() {
            session.set_title(title);
        }
        session.persist();
        self.sessions
            .lock()
            .expect("registry lock poisoned")
            .insert(id, Arc::clone(&session));
        Ok(session)
    }

    /// The cached (or lazily loaded) session for `id`. An unknown id is
    /// `ApiErrorKind::NotFound`.
    pub fn get(&self, id: &str) -> Result<Arc<ChatSession>> {
        if let Some(session) = self
            .sessions
            .lock()
            .expect("registry lock poisoned")
            .get(id)
        {
            return Ok(Arc::clone(session));
        }
        if self.store.load(id)?.is_none() {
            return Err(ApiError::new(
                ApiErrorKind::NotFound,
                format!("no thread with id {id:?}"),
            ));
        }
        let session = Arc::new(ChatSession::with_store(
            Arc::clone(&self.core),
            id.to_owned(),
            self.store.clone(),
        ));
        let mut sessions = self.sessions.lock().expect("registry lock poisoned");
        Ok(Arc::clone(sessions.entry(id.to_owned()).or_insert(session)))
    }

    /// The most recently updated thread, if any.
    pub fn latest(&self) -> Result<Option<Arc<ChatSession>>> {
        match self.store.list()?.into_iter().next() {
            Some(conversation) => Ok(Some(self.get(&conversation.id)?)),
            None => Ok(None),
        }
    }

    /// Drop the session and remove its file. Deleting the selected thread is
    /// the caller's cue to reselect (fresh or newest).
    pub fn delete(&self, id: &str) -> Result<()> {
        if let Some(session) = self
            .sessions
            .lock()
            .expect("registry lock poisoned")
            .remove(id)
        {
            // Stop a running turn first so it cannot re-create the file.
            let _ = session.abort();
        }
        self.store.delete(id)
    }
}

/// Thread id generator: no `uuid` dependency (C15). Nanoseconds since the
/// epoch plus a process-local counter keeps ids unique and file-name-safe.
fn new_thread_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("t{nanos:x}{counter:x}")
}
