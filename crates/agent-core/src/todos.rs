//! Named TODO lists (M7, ADR-029): one plain JSON file per list under
//! `data/todos/{slug}.json`, schema-versioned, atomic writes. The store is the
//! single source of truth shared by the `todo` tool (the agent's edit path) and
//! every frontend; a broken or unknown-schema file is quarantined, never fatal.
//!
//! Writes serialize on a store-wide mutex so the agent and a frontend can edit
//! one list concurrently without losing items.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiErrorKind, Result};

/// The TODO list file version this build reads and writes (M7).
pub const TODO_SCHEMA_VERSION: u32 = 1;

/// Longest slug derived from a title.
const MAX_SLUG_CHARS: usize = 40;

/// One item in a named list. `id` is stable across edits, so a check, rename or
/// remove targets the same item even if the list changed in between.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub done: bool,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// A whole named list as stored on disk (`data/todos/{slug}.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    pub schema: u32,
    pub slug: String,
    pub title: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// Stamped by [`TodoStore::save`] on every change; orders the list view.
    #[serde(rename = "updatedAt", default)]
    pub updated_at: String,
    #[serde(default)]
    pub items: Vec<TodoItem>,
}

impl TodoList {
    pub fn open_count(&self) -> usize {
        self.items.iter().filter(|item| !item.done).count()
    }

    pub fn done_count(&self) -> usize {
        self.items.iter().filter(|item| item.done).count()
    }
}

/// A list header for `GET /api/todos` and the tool's `list` action (no bodies).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TodoListSummary {
    pub slug: String,
    pub title: String,
    pub open: usize,
    pub done: usize,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

impl From<&TodoList> for TodoListSummary {
    fn from(list: &TodoList) -> Self {
        Self {
            slug: list.slug.clone(),
            title: list.title.clone(),
            open: list.open_count(),
            done: list.done_count(),
            updated_at: list.updated_at.clone(),
        }
    }
}

/// File-backed store for named TODO lists under one directory (ADR-029).
///
/// The write mutex is shared across clones (via `Arc`), so the agent tool and a
/// frontend handle to the same store serialize read-modify-write and never drop
/// a concurrent edit.
#[derive(Debug, Clone)]
pub struct TodoStore {
    dir: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl TodoStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// All list headers, most recently changed first.
    pub fn list(&self) -> Result<Vec<TodoListSummary>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(ApiError::internal(format!(
                    "cannot read todos dir {}: {e}",
                    self.dir.display()
                )));
            }
        };
        let mut lists = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                ApiError::internal(format!("cannot read todos dir {}: {e}", self.dir.display()))
            })?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Foreign file names are skipped, not fatal (`load` would error).
            if validate_slug(slug).is_err() {
                continue;
            }
            if let Some(list) = self.load(slug)? {
                lists.push(TodoListSummary::from(&list));
            }
        }
        lists.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.slug.cmp(&b.slug))
        });
        Ok(lists)
    }

    /// Load one list; a missing slug is `NotFound`.
    pub fn get(&self, slug: &str) -> Result<TodoList> {
        match self.load(slug)? {
            Some(list) => Ok(list),
            None => Err(ApiError::new(
                ApiErrorKind::NotFound,
                format!("no todo list {slug:?}"),
            )),
        }
    }

    /// Create a list from a title, deriving a unique slug.
    pub fn create(&self, title: &str) -> Result<TodoList> {
        let title = title.trim();
        if title.is_empty() {
            return Err(ApiError::config("a todo list needs a non-empty title"));
        }
        let _guard = self.lock();
        let slug = self.unique_slug(title)?;
        let now = crate::conversations::now_rfc3339();
        let list = TodoList {
            schema: TODO_SCHEMA_VERSION,
            slug,
            title: title.to_owned(),
            created_at: now.clone(),
            updated_at: now,
            items: Vec::new(),
        };
        self.save(&list)?;
        Ok(list)
    }

    /// Rename a list's display title (the slug/file name is unchanged).
    pub fn rename(&self, slug: &str, title: &str) -> Result<TodoList> {
        let title = title.trim();
        if title.is_empty() {
            return Err(ApiError::config("a todo list needs a non-empty title"));
        }
        self.modify(slug, |list| {
            list.title = title.to_owned();
            Ok(())
        })?;
        self.get(slug)
    }

    /// Delete a list; a missing file is a no-op (idempotent).
    pub fn delete(&self, slug: &str) -> Result<()> {
        let path = self.path_for(slug)?;
        let _guard = self.lock();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ApiError::internal(format!(
                "cannot delete {}: {e}",
                path.display()
            ))),
        }
    }

    pub fn add_item(&self, slug: &str, text: &str) -> Result<TodoItem> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ApiError::config("a todo item needs non-empty text"));
        }
        self.modify(slug, |list| {
            let item = TodoItem {
                id: fresh_id(list),
                text: text.to_owned(),
                done: false,
                created_at: crate::conversations::now_rfc3339(),
            };
            list.items.push(item.clone());
            Ok(item)
        })
    }

    /// Apply any combination of a new text and a new done flag to one item.
    pub fn update_item(
        &self,
        slug: &str,
        id: &str,
        text: Option<&str>,
        done: Option<bool>,
    ) -> Result<TodoItem> {
        let text = match text {
            Some(text) => {
                let text = text.trim();
                if text.is_empty() {
                    return Err(ApiError::config("a todo item needs non-empty text"));
                }
                Some(text.to_owned())
            }
            None => None,
        };
        self.modify(slug, |list| {
            let item = find_item(list, id)?;
            if let Some(text) = &text {
                item.text = text.clone();
            }
            if let Some(done) = done {
                item.done = done;
            }
            Ok(item.clone())
        })
    }

    pub fn set_done(&self, slug: &str, id: &str, done: bool) -> Result<TodoItem> {
        self.update_item(slug, id, None, Some(done))
    }

    pub fn rename_item(&self, slug: &str, id: &str, text: &str) -> Result<TodoItem> {
        self.update_item(slug, id, Some(text), None)
    }

    pub fn remove_item(&self, slug: &str, id: &str) -> Result<()> {
        self.modify(slug, |list| {
            let before = list.items.len();
            list.items.retain(|item| item.id != id);
            if list.items.len() == before {
                return Err(ApiError::new(
                    ApiErrorKind::NotFound,
                    format!("no item {id:?} in list {slug:?}"),
                ));
            }
            Ok(())
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read-modify-write one list under the store lock, stamping `updatedAt`.
    fn modify<T>(&self, slug: &str, change: impl FnOnce(&mut TodoList) -> Result<T>) -> Result<T> {
        let _guard = self.lock();
        let mut list = self.get(slug)?;
        let outcome = change(&mut list)?;
        self.save(&list)?;
        Ok(outcome)
    }

    fn unique_slug(&self, title: &str) -> Result<String> {
        let base = slugify(title);
        if !self.path_for(&base)?.exists() {
            return Ok(base);
        }
        for suffix in 2..=9999u32 {
            let candidate = format!("{base}-{suffix}");
            if !self.path_for(&candidate)?.exists() {
                return Ok(candidate);
            }
        }
        Err(ApiError::internal("too many todo lists with the same name"))
    }

    fn save(&self, list: &TodoList) -> Result<()> {
        validate_slug(&list.slug)?;
        let mut stamped = list.clone();
        stamped.schema = TODO_SCHEMA_VERSION;
        stamped.updated_at = crate::conversations::now_rfc3339();
        let text = serde_json::to_string_pretty(&stamped)
            .map_err(|e| ApiError::internal(format!("cannot serialize todo list: {e}")))?;
        crate::util::write_atomic(&self.path_for(&list.slug)?, &text)
    }

    fn load(&self, slug: &str) -> Result<Option<TodoList>> {
        let path = self.path_for(slug)?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ApiError::internal(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        match migrate(&text) {
            Ok(list) => Ok(Some(list)),
            Err(err) => {
                tracing::warn!(
                    target: "agent_core::todos",
                    "todo file {} is not loadable ({err}); quarantining it",
                    path.display()
                );
                self.quarantine(&path);
                Ok(None)
            }
        }
    }

    fn path_for(&self, slug: &str) -> Result<PathBuf> {
        validate_slug(slug)?;
        Ok(self.dir.join(format!("{slug}.json")))
    }

    fn quarantine(&self, path: &Path) {
        let quarantined = path.with_extension("json.quarantine");
        match std::fs::rename(path, &quarantined) {
            Ok(()) => tracing::warn!(
                target: "agent_core::todos",
                "moved {} out of the way (fix or delete it by hand)",
                quarantined.display()
            ),
            Err(e) => tracing::warn!(
                target: "agent_core::todos",
                "cannot quarantine {}: {e}; it will fail again on the next load",
                path.display()
            ),
        }
    }
}

/// Migration hook (§5.4): older `schema` values are upgraded here; newer or
/// malformed ones are a load error (which the store turns into a quarantine).
/// v1 is the first version, so there are no steps yet.
fn migrate(text: &str) -> Result<TodoList> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        ApiError::new(
            ApiErrorKind::Internal,
            format!("not a valid todo file: {e}"),
        )
    })?;
    let schema = value
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| ApiError::internal("todo file has no schema version"))?;
    if schema == 0 {
        return Err(ApiError::internal(
            "schema version 0 is not a valid todo file",
        ));
    }
    // Compare before narrowing: a crafted `schema = 2^32 + 1` must not wrap.
    if schema > u64::from(TODO_SCHEMA_VERSION) {
        return Err(ApiError::internal(format!(
            "schema version {schema} is unknown (this build speaks {TODO_SCHEMA_VERSION})"
        )));
    }
    serde_json::from_value(value).map_err(|e| {
        ApiError::new(
            ApiErrorKind::Internal,
            format!("not a valid todo file: {e}"),
        )
    })
}

/// A title becomes a single-component file slug: lowercase ASCII letters,
/// digits and `-`. Never `""`, `.`/`..`, or anything with a separator.
fn slugify(title: &str) -> String {
    let mut slug: String = title
        .chars()
        .take(MAX_SLUG_CHARS)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "list".to_owned()
    } else {
        slug.to_owned()
    }
}

/// Slugs become file names: keep them to plain `[a-z0-9_-]` so no path tricks
/// are possible, no matter which frontend invents them.
fn validate_slug(slug: &str) -> Result<()> {
    let valid = !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(ApiError::config(format!(
            "todo list name {slug:?} is invalid (allowed: letters, digits, '-', '_', max 64 chars)"
        )))
    }
}

fn find_item<'a>(list: &'a mut TodoList, id: &str) -> Result<&'a mut TodoItem> {
    list.items
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorKind::NotFound,
                format!("no item {id:?} in list {:?}", list.slug),
            )
        })
}

/// A short opaque item id, unique within the list (the store lock makes the
/// nanosecond clock effectively collision-free; the loop is belt and braces).
fn fresh_id(list: &TodoList) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut id = format!("{nanos:x}");
    let mut attempt = 0u32;
    while list.items.iter().any(|item| item.id == id) {
        attempt += 1;
        id = format!("{nanos:x}-{attempt}");
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> TodoStore {
        TodoStore::new(crate::util::temp_dir("todos", name))
    }

    #[test]
    fn create_list_add_check_and_reload() {
        let store = temp_store("round-trip");
        let list = store.create("Shopping list").unwrap();
        assert_eq!(list.slug, "shopping-list");
        assert_eq!(list.title, "Shopping list");

        let item = store.add_item("shopping-list", "oat milk").unwrap();
        assert!(!item.done);
        let item = store.set_done("shopping-list", &item.id, true).unwrap();
        assert!(item.done);

        // A fresh store over the same dir sees the same data.
        let reopened = TodoStore::new(store.dir());
        let list = reopened.get("shopping-list").unwrap();
        assert_eq!(list.title, "Shopping list");
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].text, "oat milk");
        assert!(list.items[0].done);
        assert_eq!(list.open_count(), 0);
        assert_eq!(list.done_count(), 1);
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn titles_become_unique_slugs() {
        let store = temp_store("slugs");
        assert_eq!(store.create("Shopping list").unwrap().slug, "shopping-list");
        assert_eq!(
            store.create("Shopping list").unwrap().slug,
            "shopping-list-2"
        );
        assert_eq!(store.create("Projects!").unwrap().slug, "projects");
        // A title with nothing usable still yields a valid slug.
        assert_eq!(store.create("###").unwrap().slug, "list");
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn rename_remove_and_delete() {
        let store = temp_store("mutate");
        store.create("Projects").unwrap();
        let item = store.add_item("projects", "write M7").unwrap();
        let renamed = store.rename_item("projects", &item.id, "ship M7").unwrap();
        assert_eq!(renamed.text, "ship M7");
        assert_eq!(store.get("projects").unwrap().title, "Projects");

        let renamed_list = store.rename("projects", "Work").unwrap();
        assert_eq!(renamed_list.title, "Work");
        assert_eq!(renamed_list.slug, "projects", "the file name stays");

        store.remove_item("projects", &item.id).unwrap();
        assert!(store.get("projects").unwrap().items.is_empty());

        store.delete("projects").unwrap();
        assert!(store.get("projects").is_err());
        // Deleting again is a no-op.
        store.delete("projects").unwrap();
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn list_is_sorted_newest_first_and_summarizes_counts() {
        let store = temp_store("list");
        store.create("First").unwrap();
        store.create("Second").unwrap();
        store.add_item("second", "a").unwrap();
        store.add_item("second", "b").unwrap();
        let summaries = store.list().unwrap();
        assert_eq!(summaries.len(), 2);
        let second = summaries.iter().find(|s| s.slug == "second").unwrap();
        assert_eq!(second.open, 2);
        assert_eq!(second.done, 0);
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn missing_lists_and_items_are_not_found() {
        let store = temp_store("missing");
        assert_eq!(store.get("nope").unwrap_err().kind, ApiErrorKind::NotFound);
        store.create("Shopping").unwrap();
        assert_eq!(
            store.set_done("shopping", "ghost", true).unwrap_err().kind,
            ApiErrorKind::NotFound
        );
        assert_eq!(
            store.remove_item("shopping", "ghost").unwrap_err().kind,
            ApiErrorKind::NotFound
        );
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn traversal_shaped_slugs_are_rejected() {
        let store = temp_store("traversal");
        for slug in ["../secret", "a/b", ".hidden", "", ".."] {
            assert!(
                store.get(slug).is_err(),
                "{slug:?} must not resolve to a file"
            );
        }
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn broken_and_unknown_schema_files_are_quarantined() {
        let store = temp_store("quarantine");
        std::fs::create_dir_all(store.dir()).unwrap();
        std::fs::write(store.dir().join("broken.json"), "{ not json").unwrap();
        std::fs::write(
            store.dir().join("future.json"),
            r#"{"schema":99,"slug":"future","title":"x"}"#,
        )
        .unwrap();

        let lists = store.list().unwrap();
        assert!(lists.is_empty());
        assert!(store.dir().join("broken.json.quarantine").is_file());
        assert!(store.dir().join("future.json.quarantine").is_file());
        assert!(!store.dir().join("broken.json").exists());
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn saves_are_atomic_and_leave_no_tmp_files() {
        let store = temp_store("atomic");
        store.create("Shopping").unwrap();
        let names: Vec<String> = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["shopping.json".to_string()]);
        std::fs::remove_dir_all(store.dir()).ok();
    }

    #[test]
    fn concurrent_writers_lose_no_items() {
        let store = temp_store("concurrent");
        store.create("Shared").unwrap();
        let mut handles = Vec::new();
        for i in 0..8 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                store.add_item("shared", &format!("item {i}")).unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(store.get("shared").unwrap().items.len(), 8);
        std::fs::remove_dir_all(store.dir()).ok();
    }
}
